//! viewport normalisation: collapse the Option-heavy viewport KVP slice
//! (LAYERS, CRS, BBOX, WIDTH/HEIGHT, FORMAT, DPI) into a fully-validated
//! [`mars_runtime::RenderPlan`].
//!
//! shared by `prepare::get_map` and `prepare::get_feature_info`; each op
//! owns its extension fields (EXCEPTIONS / i,j/info_format/...) around this
//! core. mirrors `mars-render/src/prepare.rs::resolve` - a single chokepoint
//! where allowlist, bound, axis-order, and bbox-shape checks live so
//! downstream consumers never re-validate.

use mars_proj::{AxisOrder, axis_order};
use mars_runtime::RenderPlan;
use mars_types::{Bbox, CrsCode, ImageFormat, LayerId};

use crate::{WmsConfig, WmsError, WmsOperation, WmsVersion};

/// option-heavy viewport slice produced by the parse layer.
#[derive(Debug, Default, Clone)]
pub(crate) struct ParsedViewport {
    pub layers: Option<Vec<LayerId>>,
    pub crs: Option<String>,
    pub bbox: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub format: Option<String>,
    /// raw `&DPI=` (or `&MAP_RESOLUTION=`) value; per-request override of the
    /// service-default scale dpi.
    pub dpi: Option<f64>,
}

pub(crate) fn resolve_viewport(
    p: &ParsedViewport,
    cfg: &WmsConfig,
    version: WmsVersion,
    gate_op: WmsOperation,
) -> Result<RenderPlan, WmsError> {
    let layers = p.layers.as_ref().ok_or(WmsError::MissingParam("layers"))?.clone();
    if layers.is_empty() {
        return Err(WmsError::InvalidParam {
            name: "layers",
            reason: "no layer names".into(),
        });
    }
    if layers.len() > cfg.max_layers {
        return Err(WmsError::InvalidParam {
            name: "layers",
            reason: format!("{} exceeds max {}", layers.len(), cfg.max_layers),
        });
    }
    // gfi pre-filters denied layers in its own resolver, so by the time the
    // call lands here every layer must permit `gate_op`; gating with a
    // single per-op parameter keeps the chokepoint shape uniform.
    for layer in &layers {
        if !cfg.permits(layer, gate_op) {
            return Err(WmsError::OperationNotPermitted {
                layer: layer.clone(),
                op: gate_op,
            });
        }
    }

    let crs_raw = p.crs.as_deref().ok_or(WmsError::MissingParam("crs"))?;
    if !cfg.allowlist_crs.is_empty() && !cfg.allowlist_crs.iter().any(|c| c.as_str() == crs_raw) {
        return Err(WmsError::InvalidParam {
            name: "crs",
            reason: format!("{crs_raw} not in reprojection allowlist"),
        });
    }
    let crs = CrsCode::new(crs_raw);

    let bbox_raw = p.bbox.as_deref().ok_or(WmsError::MissingParam("bbox"))?;
    let bbox = resolve_bbox(bbox_raw, &crs, version, cfg.max_bbox_coord)?;

    let width = p.width.ok_or(WmsError::MissingParam("width"))?;
    let height = p.height.ok_or(WmsError::MissingParam("height"))?;
    if width == 0 || height == 0 {
        return Err(WmsError::InvalidParam {
            name: "width|height",
            reason: "must be > 0".into(),
        });
    }
    if width > cfg.max_image_dimension || height > cfg.max_image_dimension {
        return Err(WmsError::InvalidParam {
            name: "width|height",
            reason: format!("max dimension is {}, got {}x{}", cfg.max_image_dimension, width, height),
        });
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > cfg.max_pixels {
        return Err(WmsError::InvalidParam {
            name: "width|height",
            reason: format!(
                "max pixels per request is {}, got {} ({}x{})",
                cfg.max_pixels, pixels, width, height
            ),
        });
    }

    let format_raw = p.format.as_deref().ok_or(WmsError::MissingParam("format"))?;
    let format = resolve_format(format_raw, cfg)?;

    let scale_pixel_size_m = match p.dpi {
        Some(dpi) => {
            if !dpi.is_finite() || dpi <= 0.0 {
                return Err(WmsError::InvalidParam {
                    name: "dpi",
                    reason: "must be a positive, finite number".into(),
                });
            }
            0.0254 / dpi
        }
        None => cfg.scale_pixel_size_m,
    };

    Ok(RenderPlan {
        layers,
        bbox,
        width,
        height,
        crs,
        format,
        scale_pixel_size_m,
    })
}

fn resolve_format(raw: &str, cfg: &WmsConfig) -> Result<ImageFormat, WmsError> {
    let format = ImageFormat::from_mime(raw).ok_or_else(|| WmsError::InvalidParam {
        name: "format",
        reason: format!("unsupported {raw}"),
    })?;
    if !cfg.formats.is_empty() && !cfg.formats.contains(&format) {
        return Err(WmsError::InvalidParam {
            name: "format",
            reason: format!("{raw} not enabled"),
        });
    }
    Ok(format)
}

/// Axis-order rule:
/// - WMS 1.3.0: obey the CRS-declared axis order via [`axis_order`].
///   Geographic CRSes (EPSG:4326, EPSG:4258, ...) advertise north/east, so
///   the wire is `miny,minx,maxy,maxx`; projected CRSes use east/north and
///   the natural `minx,miny,maxx,maxy`.
/// - WMS 1.1.1: always east/north on the wire regardless of CRS, matching
///   pre-1.3.0 OGC convention and what legacy clients still send.
fn resolve_bbox(raw: &str, crs: &CrsCode, version: WmsVersion, max_coord: f64) -> Result<Bbox, WmsError> {
    let parts: Vec<&str> = raw.split(',').collect();
    if parts.len() != 4 {
        return Err(WmsError::InvalidParam {
            name: "bbox",
            reason: "expected 4 comma-separated floats".into(),
        });
    }
    let nums: Vec<f64> = parts
        .iter()
        .map(|s| s.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|e| WmsError::InvalidParam {
            name: "bbox",
            reason: e.to_string(),
        })?;
    let order = match version {
        WmsVersion::V111 => AxisOrder::EastNorth,
        WmsVersion::V130 => axis_order(crs).map_err(|e| WmsError::InvalidParam {
            name: "crs",
            reason: format!("axis order lookup failed: {e}"),
        })?,
    };
    let (min_x, min_y, max_x, max_y) = match order {
        AxisOrder::NorthEast => (nums[1], nums[0], nums[3], nums[2]),
        AxisOrder::EastNorth => (nums[0], nums[1], nums[2], nums[3]),
    };
    for v in [min_x, min_y, max_x, max_y] {
        if !v.is_finite() {
            return Err(WmsError::InvalidParam {
                name: "bbox",
                reason: "coordinates must be finite".into(),
            });
        }
        if v.abs() > max_coord {
            return Err(WmsError::InvalidParam {
                name: "bbox",
                reason: format!("coordinate magnitude exceeds {max_coord}"),
            });
        }
    }
    if !(max_x > min_x && max_y > min_y) {
        return Err(WmsError::InvalidParam {
            name: "bbox",
            reason: "max must exceed min on both axes".into(),
        });
    }
    Ok(Bbox::new(min_x, min_y, max_x, max_y))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeMap;

    use mars_types::ImageFormat;

    use super::*;
    use crate::LayerPolicy;

    fn cfg_with_policies(policies: &[(&str, LayerPolicy)]) -> WmsConfig {
        let layer_policies = policies.iter().map(|(n, p)| (LayerId::new(*n), *p)).collect();
        WmsConfig {
            allowlist_crs: vec![CrsCode::new("EPSG:25832")],
            formats: vec![ImageFormat::Png],
            max_image_dimension: 8192,
            max_pixels: 16_000_000,
            max_layers: 100,
            max_bbox_coord: 1e9,
            scale_pixel_size_m: 0.0254 / 96.0,
            layer_policies,
        }
    }

    fn policy_all_allowed() -> LayerPolicy {
        LayerPolicy {
            get_map: true,
            get_capabilities: true,
            get_feature_info: true,
            get_legend_graphic: true,
        }
    }

    fn happy_viewport(layer: &str) -> ParsedViewport {
        ParsedViewport {
            layers: Some(vec![LayerId::new(layer)]),
            crs: Some("EPSG:25832".into()),
            bbox: Some("0,0,1,1".into()),
            width: Some(1),
            height: Some(1),
            format: Some("image/png".into()),
            dpi: None,
        }
    }

    #[test]
    fn resolve_viewport_denies_getmap_for_gated_layer() {
        let denied = LayerPolicy {
            get_map: false,
            ..policy_all_allowed()
        };
        let cfg = cfg_with_policies(&[("a", denied)]);
        let err = resolve_viewport(&happy_viewport("a"), &cfg, WmsVersion::V130, WmsOperation::GetMap).unwrap_err();
        assert!(matches!(
            err,
            WmsError::OperationNotPermitted {
                op: WmsOperation::GetMap,
                ..
            }
        ));
    }

    #[test]
    fn resolve_viewport_passes_getmap_for_allowed_layer() {
        let cfg = cfg_with_policies(&[("a", policy_all_allowed())]);
        let plan = resolve_viewport(&happy_viewport("a"), &cfg, WmsVersion::V130, WmsOperation::GetMap).unwrap();
        assert_eq!(plan.layers[0].as_str(), "a");
    }

    #[test]
    fn resolve_viewport_unknown_layer_passes_gate() {
        // unknown layers fall through to layer-existence check downstream;
        // the gate must not error on them.
        let cfg = WmsConfig {
            allowlist_crs: vec![CrsCode::new("EPSG:25832")],
            formats: vec![ImageFormat::Png],
            max_image_dimension: 8192,
            max_pixels: 16_000_000,
            max_layers: 100,
            max_bbox_coord: 1e9,
            scale_pixel_size_m: 0.0254 / 96.0,
            layer_policies: BTreeMap::new(),
        };
        let plan = resolve_viewport(&happy_viewport("ghost"), &cfg, WmsVersion::V130, WmsOperation::GetMap).unwrap();
        assert_eq!(plan.layers[0].as_str(), "ghost");
    }
}

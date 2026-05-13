//! viewport normalisation: collapse the Option-heavy viewport KVP slice
//! (LAYERS, CRS, BBOX, WIDTH/HEIGHT, FORMAT, DPI) into a fully-validated
//! [`mars_runtime::RenderPlan`].
//!
//! shared by `prepare::get_map` and `prepare::get_feature_info`; each op
//! owns its extension fields (EXCEPTIONS / i,j/info_format/...) around this
//! core. mirrors `mars-render/src/prepare.rs::resolve` - a single chokepoint
//! where allowlist, bound, axis-order, and bbox-shape checks live so
//! downstream consumers never re-validate.

use mars_runtime::RenderPlan;
use mars_types::{Bbox, CrsCode, ImageFormat, LayerId};

use crate::{WmsConfig, WmsError};

/// option-heavy viewport slice produced by the parse layer.
#[derive(Debug, Default, Clone)]
pub(crate) struct ParsedViewport {
    pub version: Option<String>,
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

pub(crate) fn resolve_viewport(p: &ParsedViewport, cfg: &WmsConfig) -> Result<RenderPlan, WmsError> {
    if let Some(v) = &p.version
        && v != "1.3.0"
    {
        return Err(WmsError::InvalidParam {
            name: "version",
            reason: format!("only 1.3.0 supported, got {v}"),
        });
    }

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

    let crs_raw = p.crs.as_deref().ok_or(WmsError::MissingParam("crs"))?;
    if !cfg.allowlist_crs.is_empty() && !cfg.allowlist_crs.iter().any(|c| c.as_str() == crs_raw) {
        return Err(WmsError::InvalidParam {
            name: "crs",
            reason: format!("{crs_raw} not in reprojection allowlist"),
        });
    }
    let crs = CrsCode::new(crs_raw);

    let bbox_raw = p.bbox.as_deref().ok_or(WmsError::MissingParam("bbox"))?;
    let bbox = resolve_bbox(bbox_raw, crs_raw, cfg.max_bbox_coord)?;

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
    let format = match raw {
        "image/png" => ImageFormat::Png,
        "image/jpeg" | "image/jpg" => ImageFormat::Jpeg,
        other => {
            return Err(WmsError::InvalidParam {
                name: "format",
                reason: format!("unsupported {other}"),
            });
        }
    };
    if !cfg.formats.is_empty() && !cfg.formats.contains(&format) {
        return Err(WmsError::InvalidParam {
            name: "format",
            reason: format!("{raw} not enabled"),
        });
    }
    Ok(format)
}

/// WMS 1.3.0 axis-order rule: for CRSes with lat/lon (north/east) axis order
/// the wire is `miny,minx,maxy,maxx`; for east/north it is the natural
/// `minx,miny,maxx,maxy`. only meaningful once the CRS is known, so this
/// lives in prepare rather than parse.
fn resolve_bbox(raw: &str, crs: &str, max_coord: f64) -> Result<Bbox, WmsError> {
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
    let (min_x, min_y, max_x, max_y) = if is_lat_lon_order(crs) {
        (nums[1], nums[0], nums[3], nums[2])
    } else {
        (nums[0], nums[1], nums[2], nums[3])
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

fn is_lat_lon_order(crs: &str) -> bool {
    matches!(crs, "EPSG:4326" | "urn:ogc:def:crs:EPSG::4326")
}

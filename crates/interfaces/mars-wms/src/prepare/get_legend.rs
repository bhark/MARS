//! GetLegendGraphic normalisation: takes a [`super::ParsedGetLegend`] and
//! produces a validated [`ResolvedGetLegend`]. Thin today; the wrapper
//! exists so future SLD-style swatch defaults land in one place rather
//! than being grafted onto [`mars_runtime::LegendPlan`].

use mars_runtime::LegendPlan;
use mars_types::{ImageFormat, LayerId};

use super::ParsedGetLegend;
use mars_config::ServiceOp;

use crate::{WmsConfig, WmsError};

/// Fully-validated GetLegendGraphic request.
#[derive(Debug, Clone)]
pub struct ResolvedGetLegend {
    pub plan: LegendPlan,
}

pub(crate) fn resolve_get_legend(p: ParsedGetLegend, cfg: &WmsConfig) -> Result<ResolvedGetLegend, WmsError> {
    let layer = p.layer.ok_or(WmsError::MissingParam("layer"))?;
    let layer = LayerId::new(layer);
    if !cfg.permits(&layer, ServiceOp::WmsGetLegendGraphic) {
        return Err(WmsError::OperationNotPermitted {
            layer,
            op: ServiceOp::WmsGetLegendGraphic,
        });
    }

    let format_raw = p.format.as_deref().ok_or(WmsError::MissingParam("format"))?;
    let format = resolve_format(format_raw, cfg)?;

    let swatch_width = p.width.unwrap_or(LegendPlan::DEFAULT_SWATCH_WIDTH);
    let swatch_height = p.height.unwrap_or(LegendPlan::DEFAULT_SWATCH_HEIGHT);
    if swatch_width == 0 || swatch_height == 0 {
        return Err(WmsError::InvalidParam {
            name: "width|height",
            reason: "must be > 0".into(),
        });
    }
    if swatch_width > cfg.max_image_dimension || swatch_height > cfg.max_image_dimension {
        return Err(WmsError::InvalidParam {
            name: "width|height",
            reason: format!("max dimension is {}", cfg.max_image_dimension),
        });
    }

    Ok(ResolvedGetLegend {
        plan: LegendPlan {
            layer,
            format,
            swatch_width,
            swatch_height,
            rule: p.rule,
        },
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use mars_types::{CrsCode, ImageFormat};

    use super::*;

    fn cfg() -> WmsConfig {
        WmsConfig {
            allowlist_crs: vec![CrsCode::new("EPSG:25832")],
            formats: vec![ImageFormat::Png],
            max_image_dimension: 8192,
            max_pixels: 16_000_000,
            max_layers: 100,
            max_bbox_coord: 1e9,
            scale_pixel_size_m: 0.0254 / 96.0,
            layer_policies: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn happy_defaults() {
        let p = ParsedGetLegend {
            layer: Some("roads".into()),
            format: Some("image/png".into()),
            width: None,
            height: None,
            rule: None,
        };
        let r = resolve_get_legend(p, &cfg()).unwrap();
        assert_eq!(r.plan.layer.as_str(), "roads");
        assert_eq!(r.plan.swatch_width, LegendPlan::DEFAULT_SWATCH_WIDTH);
        assert_eq!(r.plan.swatch_height, LegendPlan::DEFAULT_SWATCH_HEIGHT);
    }

    #[test]
    fn missing_layer_reports_missing() {
        let p = ParsedGetLegend {
            layer: None,
            format: Some("image/png".into()),
            width: None,
            height: None,
            rule: None,
        };
        let err = resolve_get_legend(p, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::MissingParam("layer")));
    }

    #[test]
    fn rule_passthrough() {
        let p = ParsedGetLegend {
            layer: Some("roads".into()),
            format: Some("image/png".into()),
            width: None,
            height: None,
            rule: Some("main".into()),
        };
        let r = resolve_get_legend(p, &cfg()).unwrap();
        assert_eq!(r.plan.rule.as_deref(), Some("main"));
    }

    #[test]
    fn swatch_overrides_applied() {
        let p = ParsedGetLegend {
            layer: Some("roads".into()),
            format: Some("image/png".into()),
            width: Some(40),
            height: Some(15),
            rule: None,
        };
        let r = resolve_get_legend(p, &cfg()).unwrap();
        assert_eq!(r.plan.swatch_width, 40);
        assert_eq!(r.plan.swatch_height, 15);
    }

    #[test]
    fn swatch_zero_rejected() {
        let p = ParsedGetLegend {
            layer: Some("roads".into()),
            format: Some("image/png".into()),
            width: Some(0),
            height: None,
            rule: None,
        };
        let err = resolve_get_legend(p, &cfg()).unwrap_err();
        assert!(matches!(
            err,
            WmsError::InvalidParam {
                name: "width|height",
                ..
            }
        ));
    }

    #[test]
    fn legend_denied_when_get_legend_graphic_gated() {
        use mars_types::LayerId;
        let mut c = cfg();
        c.layer_policies.insert(
            LayerId::new("roads"),
            crate::LayerPolicy {
                get_map: true,
                get_capabilities: true,
                get_feature_info: true,
                get_legend_graphic: false,
            },
        );
        let p = ParsedGetLegend {
            layer: Some("roads".into()),
            format: Some("image/png".into()),
            width: None,
            height: None,
            rule: None,
        };
        let err = resolve_get_legend(p, &c).unwrap_err();
        assert!(matches!(
            err,
            WmsError::OperationNotPermitted {
                op: ServiceOp::WmsGetLegendGraphic,
                ..
            }
        ));
    }
}

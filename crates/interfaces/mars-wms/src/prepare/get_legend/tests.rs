#![allow(clippy::unwrap_used, clippy::panic)]

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

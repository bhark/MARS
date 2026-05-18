#![allow(clippy::unwrap_used)]

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
    let err = resolve_viewport(&happy_viewport("a"), &cfg, WmsVersion::V130, ServiceOp::WmsGetMap).unwrap_err();
    assert!(matches!(
        err,
        WmsError::OperationNotPermitted {
            op: ServiceOp::WmsGetMap,
            ..
        }
    ));
}

#[test]
fn resolve_viewport_passes_getmap_for_allowed_layer() {
    let cfg = cfg_with_policies(&[("a", policy_all_allowed())]);
    let plan = resolve_viewport(&happy_viewport("a"), &cfg, WmsVersion::V130, ServiceOp::WmsGetMap).unwrap();
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
    let plan = resolve_viewport(&happy_viewport("ghost"), &cfg, WmsVersion::V130, ServiceOp::WmsGetMap).unwrap();
    assert_eq!(plan.layers[0].as_str(), "ghost");
}

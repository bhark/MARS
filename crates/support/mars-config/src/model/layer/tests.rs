#![allow(clippy::unwrap_used)]

use mars_types::LayerId;

use super::*;

fn bare_layer() -> Layer {
    Layer {
        name: LayerId::new("a"),
        title: String::new(),
        abstract_: String::new(),
        kind: "polygon".into(),
        scale: None,
        group: None,
        bbox: None,
        sources: Vec::new(),
        classes: Vec::new(),
        label: None,
        label_survival: LabelSurvival::default(),
        raster: None,
        wms: LayerWms::default(),
        ows: LayerOws::default(),
        template: None,
    }
}

#[test]
fn permits_op_defaults_allow_for_wms_non_gfi() {
    let l = bare_layer();
    assert!(l.permits_op(ServiceOp::WmsGetMap));
    assert!(l.permits_op(ServiceOp::WmsGetCapabilities));
    assert!(l.permits_op(ServiceOp::WmsGetLegendGraphic));
}

#[test]
fn permits_op_defaults_deny_gfi_until_legacy_opt_in() {
    let mut l = bare_layer();
    assert!(!l.permits_op(ServiceOp::WmsGetFeatureInfo));
    l.wms.enable_get_feature_info = true;
    assert!(l.permits_op(ServiceOp::WmsGetFeatureInfo));
}

#[test]
fn permits_op_ows_gating_overrides_legacy_paths() {
    let mut l = bare_layer();
    l.wms.enable_get_feature_info = true;
    l.ows.request_gating.insert(ServiceOp::WmsGetFeatureInfo, false);
    assert!(!l.permits_op(ServiceOp::WmsGetFeatureInfo));

    l.ows.request_gating.insert(ServiceOp::WmsGetMap, false);
    assert!(!l.permits_op(ServiceOp::WmsGetMap));
}

#[test]
fn permits_op_wmts_defaults_allow() {
    let l = bare_layer();
    assert!(l.permits_op(ServiceOp::WmtsGetTile));
    assert!(l.permits_op(ServiceOp::WmtsGetCapabilities));
    assert!(l.permits_op(ServiceOp::WmtsGetFeatureInfo));
}

#[test]
fn permits_op_wmts_explicit_deny_wins() {
    let mut l = bare_layer();
    l.ows.request_gating.insert(ServiceOp::WmtsGetTile, false);
    assert!(!l.permits_op(ServiceOp::WmtsGetTile));
}

#[test]
fn ows_request_gating_yaml_round_trip() {
    let yaml = r#"
request_gating:
  wms_get_map: false
  wmts_get_tile: false
"#;
    let parsed: LayerOws = serde_yaml_ng::from_str(yaml.trim_start_matches('\n')).unwrap();
    assert_eq!(parsed.request_gating.get(&ServiceOp::WmsGetMap), Some(&false));
    assert_eq!(parsed.request_gating.get(&ServiceOp::WmtsGetTile), Some(&false));
}

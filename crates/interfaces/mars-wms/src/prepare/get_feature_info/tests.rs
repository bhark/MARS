#![allow(clippy::unwrap_used, clippy::panic)]

use mars_types::{CrsCode, ImageFormat, LayerId};

use super::super::viewport::ParsedViewport;
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

fn cfg_with_policies(policies: &[(&str, crate::LayerPolicy)]) -> WmsConfig {
    let mut c = cfg();
    c.layer_policies = policies.iter().map(|(n, p)| (LayerId::new(*n), *p)).collect();
    c
}

fn policy_all_allowed() -> crate::LayerPolicy {
    crate::LayerPolicy {
        get_map: true,
        get_capabilities: true,
        get_feature_info: true,
        get_legend_graphic: true,
    }
}

fn parsed(
    query_layers: Vec<&str>,
    i: u32,
    j: u32,
    info_format: &str,
    feature_count: Option<u32>,
) -> ParsedGetFeatureInfo {
    ParsedGetFeatureInfo {
        viewport: ParsedViewport {
            layers: Some(vec![LayerId::new("a"), LayerId::new("b")]),
            crs: Some("EPSG:25832".into()),
            bbox: Some("0,0,100,100".into()),
            width: Some(10),
            height: Some(10),
            format: Some("image/png".into()),
            dpi: None,
        },
        query_layers: Some(query_layers.into_iter().map(LayerId::new).collect()),
        i: Some(i),
        j: Some(j),
        info_format: Some(info_format.into()),
        feature_count,
    }
}

#[test]
fn query_layers_swapped_into_plan_layers() {
    // load-bearing semantic: plan.layers must end up = query_layers,
    // not the original LAYERS list, so the runtime walks only the
    // query subset.
    let p = parsed(vec!["a"], 5, 7, "text/plain", None);
    let r = resolve_get_feature_info(p, &cfg(), WmsVersion::V130).unwrap();
    assert_eq!(r.plan.layers.len(), 1);
    assert_eq!(r.plan.layers[0].as_str(), "a");
    assert_eq!(r.i, 5);
    assert_eq!(r.j, 7);
    assert_eq!(r.info_format, InfoFormat::TextPlain);
    assert_eq!(r.feature_count, 1);
}

#[test]
fn query_layers_must_be_subset_of_layers() {
    let p = parsed(vec!["z"], 0, 0, "text/plain", None);
    let err = resolve_get_feature_info(p, &cfg(), WmsVersion::V130).unwrap_err();
    assert!(matches!(
        err,
        WmsError::InvalidParam {
            name: "query_layers",
            ..
        }
    ));
}

#[test]
fn missing_query_layers_reports_missing() {
    let mut p = parsed(vec!["a"], 0, 0, "text/plain", None);
    p.query_layers = None;
    let err = resolve_get_feature_info(p, &cfg(), WmsVersion::V130).unwrap_err();
    assert!(matches!(err, WmsError::MissingParam("query_layers")));
}

#[test]
fn pixel_out_of_viewport_rejected() {
    let p = parsed(vec!["a"], 10, 0, "text/plain", None);
    let err = resolve_get_feature_info(p, &cfg(), WmsVersion::V130).unwrap_err();
    assert!(matches!(err, WmsError::InvalidParam { name: "i|j", .. }));
}

#[test]
fn unsupported_info_format_rejected() {
    let p = parsed(vec!["a"], 0, 0, "application/vnd.ogc.gml", None);
    let err = resolve_get_feature_info(p, &cfg(), WmsVersion::V130).unwrap_err();
    assert!(matches!(
        err,
        WmsError::InvalidParam {
            name: "info_format",
            ..
        }
    ));
}

#[test]
fn feature_count_default_is_one() {
    let p = parsed(vec!["a"], 0, 0, "text/plain", None);
    let r = resolve_get_feature_info(p, &cfg(), WmsVersion::V130).unwrap();
    assert_eq!(r.feature_count, 1);
}

#[test]
fn feature_count_clamped_to_max() {
    let p = parsed(vec!["a"], 0, 0, "text/plain", Some(MAX_FEATURE_COUNT + 100));
    let r = resolve_get_feature_info(p, &cfg(), WmsVersion::V130).unwrap();
    assert_eq!(r.feature_count, MAX_FEATURE_COUNT);
}

#[test]
fn feature_count_zero_rejected() {
    let p = parsed(vec!["a"], 0, 0, "text/plain", Some(0));
    let err = resolve_get_feature_info(p, &cfg(), WmsVersion::V130).unwrap_err();
    assert!(matches!(
        err,
        WmsError::InvalidParam {
            name: "feature_count",
            ..
        }
    ));
}

#[test]
fn gfi_silently_drops_denied_query_layer() {
    let denied = crate::LayerPolicy {
        get_feature_info: false,
        ..policy_all_allowed()
    };
    let c = cfg_with_policies(&[("a", policy_all_allowed()), ("b", denied)]);
    let p = parsed(vec!["a", "b"], 0, 0, "text/plain", None);
    let r = resolve_get_feature_info(p, &c, WmsVersion::V130).unwrap();
    assert_eq!(r.plan.layers.len(), 1);
    assert_eq!(r.plan.layers[0].as_str(), "a");
}

#[test]
fn gfi_returns_error_when_all_query_layers_denied() {
    let denied = crate::LayerPolicy {
        get_feature_info: false,
        ..policy_all_allowed()
    };
    let c = cfg_with_policies(&[("a", denied), ("b", denied)]);
    let p = parsed(vec!["a", "b"], 0, 0, "text/plain", None);
    let err = resolve_get_feature_info(p, &c, WmsVersion::V130).unwrap_err();
    assert!(matches!(
        err,
        WmsError::OperationNotPermitted {
            op: ServiceOp::WmsGetFeatureInfo,
            ..
        }
    ));
}

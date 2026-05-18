#![allow(clippy::unwrap_used, clippy::panic)]

use mars_types::{CrsCode, ImageFormat, LayerId};

use super::super::{ParsedGetMap, viewport::ParsedViewport};
use super::*;

fn cfg() -> WmsConfig {
    WmsConfig {
        allowlist_crs: vec![CrsCode::new("EPSG:25832"), CrsCode::new("EPSG:4326")],
        formats: vec![ImageFormat::Png],
        max_image_dimension: 8192,
        max_pixels: 16_000_000,
        max_layers: 100,
        max_bbox_coord: 1e9,
        scale_pixel_size_m: 0.0254 / 96.0,
        layer_policies: std::collections::BTreeMap::new(),
    }
}

fn happy_viewport() -> ParsedViewport {
    ParsedViewport {
        layers: Some(vec![LayerId::new("a")]),
        crs: Some("EPSG:25832".into()),
        bbox: Some("0,0,1,1".into()),
        width: Some(1),
        height: Some(1),
        format: Some("image/png".into()),
        dpi: None,
    }
}

#[test]
fn exceptions_absent_defaults_to_xml() {
    let parsed = ParsedGetMap {
        viewport: happy_viewport(),
        exceptions: None,
    };
    let r = resolve_get_map(parsed, &cfg(), WmsVersion::V130).unwrap();
    assert_eq!(r.exceptions, ExceptionsFormat::Xml);
}

#[test]
fn exceptions_blank_keyword_accepted() {
    for kw in ["BLANK", "blank", "application/vnd.ogc.se_blank"] {
        let parsed = ParsedGetMap {
            viewport: happy_viewport(),
            exceptions: Some(kw.into()),
        };
        let r = resolve_get_map(parsed, &cfg(), WmsVersion::V130).unwrap();
        assert_eq!(r.exceptions, ExceptionsFormat::Blank, "kw={kw}");
    }
}

#[test]
fn exceptions_inimage_accepted() {
    for kw in ["INIMAGE", "inimage", "application/vnd.ogc.se_inimage"] {
        let parsed = ParsedGetMap {
            viewport: happy_viewport(),
            exceptions: Some(kw.into()),
        };
        let r = resolve_get_map(parsed, &cfg(), WmsVersion::V130).unwrap();
        assert_eq!(r.exceptions, ExceptionsFormat::Inimage, "kw={kw}");
    }
}

#[test]
fn exceptions_unknown_rejected() {
    let parsed = ParsedGetMap {
        viewport: happy_viewport(),
        exceptions: Some("GARBAGE".into()),
    };
    let err = resolve_get_map(parsed, &cfg(), WmsVersion::V130).unwrap_err();
    assert!(matches!(err, WmsError::InvalidParam { name: "exceptions", .. }));
}

#[test]
fn dpi_override_applied() {
    let mut vp = happy_viewport();
    vp.dpi = Some(72.0);
    let parsed = ParsedGetMap {
        viewport: vp,
        exceptions: None,
    };
    let r = resolve_get_map(parsed, &cfg(), WmsVersion::V130).unwrap();
    assert!((r.plan.scale_pixel_size_m - 0.0254 / 72.0).abs() < 1e-12);
}

#[test]
fn missing_layers_reports_missing() {
    let mut vp = happy_viewport();
    vp.layers = None;
    let parsed = ParsedGetMap {
        viewport: vp,
        exceptions: None,
    };
    let err = resolve_get_map(parsed, &cfg(), WmsVersion::V130).unwrap_err();
    assert!(matches!(err, WmsError::MissingParam("layers")));
}

#[test]
fn crs_not_in_allowlist_rejected() {
    let mut vp = happy_viewport();
    vp.crs = Some("EPSG:3857".into());
    let parsed = ParsedGetMap {
        viewport: vp,
        exceptions: None,
    };
    let err = resolve_get_map(parsed, &cfg(), WmsVersion::V130).unwrap_err();
    assert!(matches!(err, WmsError::InvalidParam { name: "crs", .. }));
}

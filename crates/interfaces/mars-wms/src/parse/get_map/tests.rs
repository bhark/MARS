#![allow(clippy::unwrap_used, clippy::panic)]

use mars_types::{CrsCode, ImageFormat};

use super::super::parse_request;
use super::*;
use crate::ExceptionsFormat;

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

#[test]
fn happy_path() {
    let q = "service=WMS&version=1.3.0&request=GetMap&layers=a,b&styles=&\
                 crs=EPSG:25832&bbox=100,200,300,400&width=256&height=128&format=image/png";
    let plan = parse_get_map(q, &cfg()).unwrap();
    assert_eq!(plan.layers.len(), 2);
    assert_eq!(plan.width, 256);
    assert_eq!(plan.height, 128);
    assert_eq!(plan.bbox.min_x, 100.0);
    assert_eq!(plan.bbox.max_y, 400.0);
    assert_eq!(plan.crs.as_str(), "EPSG:25832");
    assert_eq!(plan.format, ImageFormat::Png);
    assert!((plan.scale_pixel_size_m - 0.0254 / 96.0).abs() < 1e-12);
}

#[test]
fn dpi_override_per_request() {
    let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png&dpi=72";
    let plan = parse_get_map(q, &cfg()).unwrap();
    assert!((plan.scale_pixel_size_m - 0.0254 / 72.0).abs() < 1e-12);
}

#[test]
fn map_resolution_alias_accepted() {
    let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png&map_resolution=120";
    let plan = parse_get_map(q, &cfg()).unwrap();
    assert!((plan.scale_pixel_size_m - 0.0254 / 120.0).abs() < 1e-12);
}

#[test]
fn dpi_invalid_rejected() {
    let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png&dpi=-5";
    let err = parse_get_map(q, &cfg()).unwrap_err();
    assert!(matches!(err, WmsError::InvalidParam { name: "dpi", .. }));
}

#[test]
fn missing_param() {
    let q = "request=GetMap&version=1.3.0";
    let err = parse_get_map(q, &cfg()).unwrap_err();
    assert!(matches!(err, WmsError::MissingParam("layers")));
}

#[test]
fn bad_bbox() {
    let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=oops&width=1&height=1&format=image/png";
    let err = parse_get_map(q, &cfg()).unwrap_err();
    assert!(matches!(err, WmsError::InvalidParam { name: "bbox", .. }));
}

// (version_str, crs_key) — drives all parametric bodies below so the
// 1.1.1 surface gets the same coverage the 1.3.0 surface already had.
const VERSION_AXIS: &[(&str, &str)] = &[("1.3.0", "crs"), ("1.1.1", "srs")];

fn parsed_exceptions(q: &str) -> ExceptionsFormat {
    let (_, req) = parse_request(q, &cfg()).unwrap();
    match req {
        crate::WmsRequest::GetMap(r) => r.exceptions,
        _ => panic!("expected GetMap"),
    }
}

#[test]
fn bbox_axis_geographic_4326_per_version() {
    // EPSG:4326 declares north/east. 1.3.0 honours the declaration and
    // swaps the wire bbox to (min_x=20, min_y=10); 1.1.1 ignores it and
    // keeps the wire shape east/north.
    for (version, crs_key) in VERSION_AXIS {
        let q = format!(
            "request=GetMap&version={version}&layers=a&{crs_key}=EPSG:4326&\
                 bbox=10,20,11,22&width=1&height=1&format=image/png"
        );
        let plan = parse_get_map(&q, &cfg()).unwrap();
        let (min_x, min_y, max_x, max_y) = if *version == "1.3.0" {
            (20.0, 10.0, 22.0, 11.0)
        } else {
            (10.0, 20.0, 11.0, 22.0)
        };
        assert_eq!(plan.bbox.min_x, min_x, "min_x for {version}");
        assert_eq!(plan.bbox.min_y, min_y, "min_y for {version}");
        assert_eq!(plan.bbox.max_x, max_x, "max_x for {version}");
        assert_eq!(plan.bbox.max_y, max_y, "max_y for {version}");
    }
}

#[test]
fn exceptions_default_is_xml_for_both_versions() {
    for (version, crs_key) in VERSION_AXIS {
        let q = format!(
            "request=GetMap&version={version}&layers=a&{crs_key}=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png"
        );
        assert_eq!(parsed_exceptions(&q), ExceptionsFormat::Xml, "{version}");
    }
}

#[test]
fn exceptions_blank_accepted_for_both_versions() {
    for (version, crs_key) in VERSION_AXIS {
        for kw in ["BLANK", "blank", "application/vnd.ogc.se_blank"] {
            let q = format!(
                "request=GetMap&version={version}&layers=a&{crs_key}=EPSG:25832&\
                     bbox=0,0,1,1&width=1&height=1&format=image/png&exceptions={kw}"
            );
            assert_eq!(parsed_exceptions(&q), ExceptionsFormat::Blank, "{version}/{kw}");
        }
    }
}

#[test]
fn exceptions_xml_keyword_accepted_for_both_versions() {
    for (version, crs_key) in VERSION_AXIS {
        for kw in ["XML", "xml", "application/vnd.ogc.se_xml"] {
            let q = format!(
                "request=GetMap&version={version}&layers=a&{crs_key}=EPSG:25832&\
                     bbox=0,0,1,1&width=1&height=1&format=image/png&exceptions={kw}"
            );
            assert_eq!(parsed_exceptions(&q), ExceptionsFormat::Xml, "{version}/{kw}");
        }
    }
}

#[test]
fn exceptions_inimage_accepted_for_both_versions() {
    for (version, crs_key) in VERSION_AXIS {
        for kw in ["INIMAGE", "inimage", "application/vnd.ogc.se_inimage"] {
            let q = format!(
                "request=GetMap&version={version}&layers=a&{crs_key}=EPSG:25832&\
                     bbox=0,0,1,1&width=1&height=1&format=image/png&exceptions={kw}"
            );
            assert_eq!(parsed_exceptions(&q), ExceptionsFormat::Inimage, "{version}/{kw}");
        }
    }
}

#[test]
fn exceptions_unknown_rejected_for_both_versions() {
    for (version, crs_key) in VERSION_AXIS {
        let q = format!(
            "request=GetMap&version={version}&layers=a&{crs_key}=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png&exceptions=GARBAGE"
        );
        let err = parse_request(&q, &cfg()).unwrap_err();
        assert!(
            matches!(err, WmsError::InvalidParam { name: "exceptions", .. }),
            "{version}: {err:?}"
        );
    }
}

#[test]
fn crs_key_fallback_is_permissive_both_directions() {
    // parse_crs (line 63-69) intentionally falls back to the other key
    // so mildly malformed clients still resolve. lock that in: 1.3.0 with
    // only srs= and 1.1.1 with only crs= both succeed.
    for (version, only_key) in [("1.3.0", "srs"), ("1.1.1", "crs")] {
        let q = format!(
            "request=GetMap&version={version}&layers=a&{only_key}=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png"
        );
        let plan = parse_get_map(&q, &cfg()).unwrap();
        assert_eq!(plan.crs.as_str(), "EPSG:25832", "{version}/{only_key}");
    }
}

#[test]
fn width_too_large() {
    let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=9000&height=1&format=image/png";
    let err = parse_get_map(q, &cfg()).unwrap_err();
    assert!(matches!(
        err,
        WmsError::InvalidParam {
            name: "width|height",
            ..
        }
    ));
}

#[test]
fn too_many_layers() {
    let q = format!(
        "request=GetMap&version=1.3.0&layers={}&crs=EPSG:25832&\
             bbox=0,0,1,1&width=1&height=1&format=image/png",
        (0..101).map(|i| i.to_string()).collect::<Vec<_>>().join(",")
    );
    let err = parse_get_map(&q, &cfg()).unwrap_err();
    assert!(matches!(err, WmsError::InvalidParam { name: "layers", .. }));
}

#[test]
fn empty_layer_name_filtered_out() {
    let q = "request=GetMap&version=1.3.0&layers=a,,b&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png";
    let plan = parse_get_map(q, &cfg()).unwrap();
    assert_eq!(plan.layers.len(), 2);
}

#[test]
fn unsupported_format_rejected() {
    let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/tiff";
    let err = parse_get_map(q, &cfg()).unwrap_err();
    assert!(matches!(err, WmsError::InvalidParam { name: "format", .. }));
}

#[test]
fn width_at_u32_max_parseable() {
    let q = format!(
        "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
             bbox=0,0,1,1&width={}&height=1&format=image/png",
        u32::MAX
    );
    let err = parse_get_map(&q, &cfg()).unwrap_err();
    assert!(matches!(
        err,
        WmsError::InvalidParam {
            name: "width|height",
            ..
        }
    ));
}

#[test]
fn crs_not_in_allowlist_rejected() {
    let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:3857&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png";
    let err = parse_get_map(q, &cfg()).unwrap_err();
    assert!(matches!(err, WmsError::InvalidParam { name: "crs", .. }));
}

#[test]
fn webp_accepted_when_configured() {
    let mut c = cfg();
    c.formats = vec![ImageFormat::Png, ImageFormat::Webp];
    let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/webp";
    let plan = parse_get_map(q, &c).unwrap();
    assert_eq!(plan.format, ImageFormat::Webp);
}

#[test]
fn wms_111_accepts_srs_parameter() {
    // 1.1.1 uses SRS= where 1.3.0 uses CRS=. Same axis order rule but the
    // key differs on the wire.
    let q = "request=GetMap&version=1.1.1&layers=a&srs=EPSG:25832&\
                 bbox=100,200,300,400&width=256&height=128&format=image/png";
    let plan = parse_get_map(q, &cfg()).unwrap();
    assert_eq!(plan.crs.as_str(), "EPSG:25832");
    assert_eq!(plan.bbox.min_x, 100.0);
    assert_eq!(plan.bbox.max_y, 400.0);
}

#[test]
fn wms_111_accepts_crs84_with_east_north_axis() {
    // CRS:84 is lon/lat; same wire shape under both 1.1.1 and 1.3.0.
    let q = "request=GetMap&version=1.1.1&layers=a&srs=CRS:84&\
                 bbox=10,20,11,22&width=1&height=1&format=image/png";
    let mut c = cfg();
    c.allowlist_crs.push(CrsCode::new("CRS:84"));
    let plan = parse_get_map(q, &c).unwrap();
    assert_eq!(plan.bbox.min_x, 10.0);
    assert_eq!(plan.bbox.min_y, 20.0);
}

#![allow(clippy::unwrap_used, clippy::panic)]

use mars_types::{CrsCode, ImageFormat};

use super::super::parse_request;
use super::*;
use crate::MAX_FEATURE_COUNT;

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

// (version_str, crs_key, i_key, j_key)
const VERSION_AXIS: &[(&str, &str, &str, &str)] = &[("1.3.0", "crs", "i", "j"), ("1.1.1", "srs", "x", "y")];

#[test]
fn gfi_happy_path() {
    let q = "request=GetFeatureInfo&version=1.3.0&layers=a,b&styles=&crs=EPSG:25832&\
                 bbox=0,0,100,100&width=10&height=10&format=image/png&\
                 query_layers=a&info_format=text/plain&i=5&j=7";
    let gfi = parse_get_feature_info(q, &cfg()).unwrap();
    assert_eq!(gfi.plan.layers.len(), 1);
    assert_eq!(gfi.plan.layers[0].as_str(), "a");
    assert_eq!(gfi.i, 5);
    assert_eq!(gfi.j, 7);
    assert_eq!(gfi.info_format, crate::InfoFormat::TextPlain);
    assert_eq!(gfi.feature_count, 1);
}

#[test]
fn gfi_query_layers_invalid_per_version() {
    for (version, crs_key, i_key, j_key) in VERSION_AXIS {
        let q = format!(
            "request=GetFeatureInfo&version={version}&layers=a&styles=&{crs_key}=EPSG:25832&\
                 bbox=0,0,100,100&width=10&height=10&format=image/png&\
                 query_layers=z&info_format=text/plain&{i_key}=0&{j_key}=0"
        );
        let err = parse_request(&q, &cfg()).unwrap_err();
        assert!(
            matches!(
                err,
                WmsError::InvalidParam {
                    name: "query_layers",
                    ..
                }
            ),
            "{version}: {err:?}"
        );
    }
}

#[test]
fn gfi_pixel_out_of_viewport_per_version() {
    for (version, crs_key, i_key, j_key) in VERSION_AXIS {
        let q = format!(
            "request=GetFeatureInfo&version={version}&layers=a&styles=&{crs_key}=EPSG:25832&\
                 bbox=0,0,100,100&width=10&height=10&format=image/png&\
                 query_layers=a&info_format=text/plain&{i_key}=10&{j_key}=0"
        );
        let err = parse_request(&q, &cfg()).unwrap_err();
        assert!(
            matches!(err, WmsError::InvalidParam { name: "i|j", .. }),
            "{version}: {err:?}"
        );
    }
}

#[test]
fn gfi_unsupported_info_format_rejected() {
    let q = "request=GetFeatureInfo&version=1.3.0&layers=a&styles=&crs=EPSG:25832&\
                 bbox=0,0,100,100&width=10&height=10&format=image/png&\
                 query_layers=a&info_format=application/vnd.ogc.gml&i=0&j=0";
    let err = parse_request(q, &cfg()).unwrap_err();
    assert!(matches!(
        err,
        WmsError::InvalidParam {
            name: "info_format",
            ..
        }
    ));
}

#[test]
fn gfi_feature_count_clamped_to_max() {
    let q = format!(
        "request=GetFeatureInfo&version=1.3.0&layers=a&styles=&crs=EPSG:25832&\
             bbox=0,0,100,100&width=10&height=10&format=image/png&\
             query_layers=a&info_format=text/plain&i=0&j=0&feature_count={}",
        MAX_FEATURE_COUNT + 100
    );
    let gfi = parse_get_feature_info(&q, &cfg()).unwrap();
    assert_eq!(gfi.feature_count, MAX_FEATURE_COUNT);
}

#[test]
fn gfi_feature_count_zero_rejected() {
    let q = "request=GetFeatureInfo&version=1.3.0&layers=a&styles=&crs=EPSG:25832&\
                 bbox=0,0,100,100&width=10&height=10&format=image/png&\
                 query_layers=a&info_format=text/plain&i=0&j=0&feature_count=0";
    let err = parse_request(q, &cfg()).unwrap_err();
    assert!(matches!(
        err,
        WmsError::InvalidParam {
            name: "feature_count",
            ..
        }
    ));
}

#[test]
fn gfi_pixel_key_primary_per_version() {
    // 1.3.0 expects I/J with CRS=; 1.1.1 expects X/Y with SRS=. Both
    // round-trip through the same resolver and land on gfi.i / gfi.j.
    for (version, crs_key, i_key, j_key) in VERSION_AXIS {
        let q = format!(
            "request=GetFeatureInfo&version={version}&layers=a&styles=&{crs_key}=EPSG:25832&\
                 bbox=0,0,100,100&width=10&height=10&format=image/png&\
                 query_layers=a&info_format=text/plain&{i_key}=5&{j_key}=7"
        );
        let gfi = parse_get_feature_info(&q, &cfg()).unwrap();
        assert_eq!(gfi.i, 5, "{version}");
        assert_eq!(gfi.j, 7, "{version}");
        assert_eq!(gfi.plan.layers[0].as_str(), "a", "{version}");
    }
}

#[test]
fn gfi_pixel_key_fallback_per_version() {
    // permissive fallback in both directions: a 1.3.0 client sending X/Y
    // and a 1.1.1 client sending I/J both resolve rather than 400.
    for (version, crs_key, fallback_i, fallback_j) in [("1.3.0", "crs", "x", "y"), ("1.1.1", "srs", "i", "j")] {
        let q = format!(
            "request=GetFeatureInfo&version={version}&layers=a&styles=&{crs_key}=EPSG:25832&\
                 bbox=0,0,100,100&width=10&height=10&format=image/png&\
                 query_layers=a&info_format=text/plain&{fallback_i}=3&{fallback_j}=4"
        );
        let gfi = parse_get_feature_info(&q, &cfg()).unwrap();
        assert_eq!(gfi.i, 3, "{version}");
        assert_eq!(gfi.j, 4, "{version}");
    }
}

#![allow(clippy::unwrap_used, clippy::panic)]

use mars_types::{CrsCode, ImageFormat};

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

#[test]
fn dispatch_capabilities() {
    let q = "service=WMS&version=1.3.0&request=GetCapabilities";
    let (version, req) = parse_request(q, &cfg()).unwrap();
    assert_eq!(version, WmsVersion::V130);
    assert!(matches!(req, WmsRequest::GetCapabilities));
}

//! Thin re-export of the KVP helpers lifted into `mars-ows-common`. Kept as
//! a local module so existing `super::common::*` imports stay valid; new
//! callers can reach for `mars_ows_common` directly.

pub(super) use mars_ows_common::{Kvp, nonempty, parse_kvp, parse_optional_u32, require};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use mars_types::{CrsCode, ImageFormat};

    use super::super::parse_get_map;
    use crate::{WmsConfig, WmsError};

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
    fn percent_decode() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG%3A25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image%2Fpng";
        let plan = parse_get_map(q, &cfg()).unwrap();
        assert_eq!(plan.crs.as_str(), "EPSG:25832");
    }

    #[test]
    fn malformed_percent_encoding_passes_through() {
        let q = "request=GetMap&version=1.3.0&layers=foo%ZZ%G&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png";
        let plan = parse_get_map(q, &cfg()).unwrap();
        assert_eq!(plan.layers[0].as_str(), "foo%ZZ%G");
    }

    #[test]
    fn percent_decode_at_end_of_input() {
        // boundary: %xx at the very end of the value must decode (the previous
        // hand-rolled implementation had an off-by-one that emitted it literally).
        let q = "request=GetMap&version=1.3.0&layers=ab%2F&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png";
        let plan = parse_get_map(q, &cfg()).unwrap();
        assert_eq!(plan.layers[0].as_str(), "ab/");
    }

    #[test]
    fn multiple_equals_in_value() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png&custom=val=ue";
        let plan = parse_get_map(q, &cfg()).unwrap();
        assert_eq!(plan.layers.len(), 1);
    }

    #[test]
    fn bbox_non_finite() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,inf&width=1&height=1&format=image/png";
        let err = parse_get_map(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::InvalidParam { name: "bbox", .. }));
    }

    #[test]
    fn bbox_coord_too_large() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1e10&width=1&height=1&format=image/png";
        let err = parse_get_map(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::InvalidParam { name: "bbox", .. }));
    }

    #[test]
    fn bbox_max_equals_min_rejected() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,0,1&width=1&height=1&format=image/png";
        let err = parse_get_map(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::InvalidParam { name: "bbox", .. }));
    }
}

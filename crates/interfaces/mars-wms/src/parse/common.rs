//! Shared KVP-parsing helpers used by every WMS operation.
//!
//! KVP semantics: parameter names are case-insensitive (lowercased on parse,
//! per OGC 06-042 §11.5.2); values are preserved as-is. Repeated keys
//! follow last-win semantics - the spec does not pin a behaviour, so this
//! is an adapter choice that matches common WMS server practice.

use std::collections::HashMap;

use percent_encoding::percent_decode_str;

use crate::WmsError;

pub(super) type Kvp = HashMap<String, String>;

pub(super) fn parse_kvp(query: &str) -> Kvp {
    let mut out = HashMap::new();
    for pair in query.trim_start_matches('?').split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        // wms is case-insensitive on parameter names per OGC 06-042
        out.insert(k.to_ascii_lowercase(), pct_decode(v));
    }
    out
}

/// percent-decode a KVP value with `+` -> space (form-style). invalid escapes
/// pass through literally, matching the prior hand-rolled behaviour.
fn pct_decode(s: &str) -> String {
    // form-style first: + means space.
    let plus_decoded: String = s.chars().map(|c| if c == '+' { ' ' } else { c }).collect();
    percent_decode_str(&plus_decoded).decode_utf8_lossy().into_owned()
}

pub(super) fn require(kvp: &Kvp, name: &'static str) -> Result<String, WmsError> {
    kvp.get(name)
        .filter(|s| !s.is_empty())
        .cloned()
        .ok_or(WmsError::MissingParam(name))
}

/// extract a non-empty KVP value as an owned String. parse-layer counterpart
/// to `require` that does not error on absence - prepare reports MissingParam.
pub(super) fn nonempty(kvp: &Kvp, name: &str) -> Option<String> {
    kvp.get(name).filter(|s| !s.is_empty()).cloned()
}

/// extract `Option<u32>` from a KVP value: missing/empty -> `Ok(None)`;
/// present but malformed -> `WmsError::InvalidParam`. semantic `required`
/// vs `optional` distinction lives in prepare, not parse.
pub(super) fn parse_optional_u32(kvp: &Kvp, name: &'static str) -> Result<Option<u32>, WmsError> {
    let raw = match kvp.get(name) {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(None),
    };
    let n = raw
        .parse()
        .map_err(|e: std::num::ParseIntError| WmsError::InvalidParam {
            name,
            reason: e.to_string(),
        })?;
    Ok(Some(n))
}

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
        // %ZZ and %G are invalid hex → passed through literally in the value
        let q = "request=GetMap&version=1.3.0&layers=foo%ZZ%G&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png";
        let plan = parse_get_map(q, &cfg()).unwrap();
        assert_eq!(plan.layers[0].as_str(), "foo%ZZ%G");
    }

    #[test]
    fn percent_decode_at_end_of_input() {
        // boundary: %xx at the very end of the value must decode (the previous
        // hand-rolled implementation had an off-by-one that emitted it literally).
        // `%2F` decodes to `/`; we feed it as the final 3 bytes of a value.
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

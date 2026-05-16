//! Shared KVP-parsing helpers used by every OWS interface (WMS, WMTS, ...).
//!
//! KVP semantics: parameter names are case-insensitive (lowercased on parse,
//! per OGC 06-121 / 06-042 / 07-057r7); values are preserved as-is. Repeated
//! keys follow last-win semantics - no OGC spec pins a behaviour, so this is
//! an adapter choice that matches common OWS server practice.

use std::collections::HashMap;

use percent_encoding::percent_decode_str;

/// Interface-side error type contract. Each protocol crate implements this
/// on its own `*Error` enum so the shared helpers can produce typed errors
/// without depending on a single concrete error type.
pub trait OwsParseError: Sized {
    /// A required parameter was absent.
    fn missing(name: &'static str) -> Self;
    /// A parameter was present but malformed; `reason` explains why.
    fn invalid(name: &'static str, reason: String) -> Self;
}

pub type Kvp = HashMap<String, String>;

/// Lowercase parameter names, percent-decode values, drop empty segments.
/// Accepts a query string with or without a leading `?`.
pub fn parse_kvp(query: &str) -> Kvp {
    let mut out = HashMap::new();
    for pair in query.trim_start_matches('?').split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(k.to_ascii_lowercase(), pct_decode(v));
    }
    out
}

/// Percent-decode a KVP value with `+` -> space (form-style). Invalid escapes
/// pass through literally, matching the hand-rolled behaviour the per-
/// protocol parsers shipped before consolidation.
pub fn pct_decode(s: &str) -> String {
    let plus_decoded: String = s.chars().map(|c| if c == '+' { ' ' } else { c }).collect();
    percent_decode_str(&plus_decoded).decode_utf8_lossy().into_owned()
}

/// Required non-empty KVP value. Errors with [`OwsParseError::missing`] when
/// absent or empty.
pub fn require<E: OwsParseError>(kvp: &Kvp, name: &'static str) -> Result<String, E> {
    kvp.get(name)
        .filter(|s| !s.is_empty())
        .cloned()
        .ok_or_else(|| E::missing(name))
}

/// Optional non-empty KVP value. Absence / empty -> `None`; otherwise the
/// owned string. Used by prepare layers that report MissingParam themselves.
pub fn nonempty(kvp: &Kvp, name: &str) -> Option<String> {
    kvp.get(name).filter(|s| !s.is_empty()).cloned()
}

/// Optional integer KVP value. Missing / empty -> `Ok(None)`; present but
/// unparseable -> [`OwsParseError::invalid`].
pub fn parse_optional_u32<E: OwsParseError>(kvp: &Kvp, name: &'static str) -> Result<Option<u32>, E> {
    let raw = match kvp.get(name) {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(None),
    };
    let n = raw
        .parse()
        .map_err(|e: std::num::ParseIntError| E::invalid(name, e.to_string()))?;
    Ok(Some(n))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    enum TestError {
        Missing(&'static str),
        Invalid { name: &'static str, reason: String },
    }

    impl OwsParseError for TestError {
        fn missing(name: &'static str) -> Self {
            Self::Missing(name)
        }
        fn invalid(name: &'static str, reason: String) -> Self {
            Self::Invalid { name, reason }
        }
    }

    #[test]
    fn lowercases_keys_and_percent_decodes_values() {
        let kvp = parse_kvp("REQUEST=GetMap&CRS=EPSG%3A25832&Empty=");
        assert_eq!(kvp.get("request").map(String::as_str), Some("GetMap"));
        assert_eq!(kvp.get("crs").map(String::as_str), Some("EPSG:25832"));
        // empty values keep their empty form (last-win)
        assert_eq!(kvp.get("empty").map(String::as_str), Some(""));
    }

    #[test]
    fn plus_decodes_to_space() {
        assert_eq!(pct_decode("a+b%20c"), "a b c");
    }

    #[test]
    fn invalid_percent_escapes_pass_through() {
        assert_eq!(pct_decode("ab%ZZ%G"), "ab%ZZ%G");
    }

    #[test]
    fn require_returns_owned_value() {
        let kvp: Kvp = [("layer".into(), "roads".into())].into_iter().collect();
        let v = require::<TestError>(&kvp, "layer").unwrap();
        assert_eq!(v, "roads");
    }

    #[test]
    fn require_errors_when_missing() {
        let kvp: Kvp = HashMap::new();
        let e = require::<TestError>(&kvp, "layer").unwrap_err();
        assert_eq!(e, TestError::Missing("layer"));
    }

    #[test]
    fn parse_optional_u32_invalid_errors() {
        let kvp: Kvp = [("count".into(), "abc".into())].into_iter().collect();
        let e = parse_optional_u32::<TestError>(&kvp, "count").unwrap_err();
        assert!(matches!(e, TestError::Invalid { name: "count", .. }));
    }
}

#![allow(clippy::unwrap_used, clippy::panic)]

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

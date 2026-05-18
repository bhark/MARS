#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn strip_placeholders_replaces_simple_token() {
    let out = strip_placeholders("dsn: ${PG_DSN}\n");
    assert!(!out.contains("${PG_DSN}"));
    assert!(out.contains("MARS_OPERATOR_PLACEHOLDER"));
}

#[test]
fn strip_placeholders_replaces_default_token() {
    let out = strip_placeholders("dsn: ${PG_DSN:-postgres://}\n");
    assert!(!out.contains("${"));
    assert!(out.contains("MARS_OPERATOR_PLACEHOLDER"));
}

#[test]
fn strip_placeholders_keeps_double_dollar_literal() {
    let out = strip_placeholders("cost: $$5\n");
    assert_eq!(out, "cost: $5\n");
}

#[test]
fn strip_placeholders_preserves_non_ascii_content() {
    // multi-byte UTF-8 around the placeholder must round-trip untouched.
    let out = strip_placeholders("label: \"Åbenrå – ${TOKEN} 🚀\"\n");
    assert!(out.contains("Åbenrå – "));
    assert!(out.contains(" 🚀"));
    assert!(out.contains("MARS_OPERATOR_PLACEHOLDER"));
    assert!(!out.contains("${"));
}

#[test]
fn strip_placeholders_leaves_unclosed_placeholder_literal() {
    let out = strip_placeholders("a${UNCLOSED");
    assert_eq!(out, "a${UNCLOSED");
}

#[test]
fn canonicalize_yaml_round_trips() {
    let v = serde_json::json!({"b": 1, "a": 2});
    let s = canonicalize_yaml(&v).unwrap();
    // both keys present; serialisation succeeds. exact ordering depends
    // on serde_json::Value which preserves insertion order in our deps,
    // so we don't assert key ordering here.
    assert!(s.contains("a:"));
    assert!(s.contains("b:"));
}

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use mars_expr::Literal;

#[test]
fn regex_with_classitem_emits_classitem_aware_predicate() {
    let w = resolve_when(
        Some(ParsedExpression::Regex {
            pattern: "^A".into(),
            case_insensitive: false,
        }),
        Some("rtt"),
        None,
        "layer",
        1,
    );
    assert_eq!(w.as_deref(), Some("rtt ~ '^A'"));
}

#[test]
fn regex_without_classitem_emits_false() {
    let w = resolve_when(
        Some(ParsedExpression::Regex {
            pattern: "foo".into(),
            case_insensitive: false,
        }),
        None,
        None,
        "layer",
        1,
    );
    // untranslatable -> when:false so the YAML still parses; the raw
    // pattern is surfaced via the stderr `warn!` channel.
    assert_eq!(w.as_deref(), Some("false"));
}

#[test]
fn regex_pattern_with_quote_is_doubled() {
    let w = resolve_when(
        Some(ParsedExpression::Regex {
            pattern: "o'brien".into(),
            case_insensitive: false,
        }),
        Some("name"),
        None,
        "layer",
        1,
    );
    assert_eq!(w.as_deref(), Some("name ~ 'o''brien'"));
}

#[test]
fn regex_case_insensitive_lifts_to_tilde_star() {
    let w = resolve_when(
        Some(ParsedExpression::Regex {
            pattern: "highway".into(),
            case_insensitive: true,
        }),
        Some("kind"),
        None,
        "layer",
        1,
    );
    assert_eq!(w.as_deref(), Some("kind ~* 'highway'"));
}

#[test]
fn closed_range_with_classitem_emits_bounded_predicate() {
    let w = resolve_when(
        Some(ParsedExpression::Range {
            lo: Literal::Int(2),
            hi: Some(Literal::Int(12)),
        }),
        Some("rtt"),
        None,
        "layer",
        1,
    );
    let s = w.unwrap();
    assert_eq!(s, "(rtt >= 2 AND rtt <= 12)");
    // round-trips through mars_expr
    mars_expr::parse(&s).unwrap();
}

#[test]
fn open_upper_range_with_classitem_emits_lower_bound_only() {
    let w = resolve_when(
        Some(ParsedExpression::Range {
            lo: Literal::Int(100),
            hi: None,
        }),
        Some("rtt"),
        None,
        "layer",
        1,
    );
    let s = w.unwrap();
    assert_eq!(s, "rtt >= 100");
    mars_expr::parse(&s).unwrap();
}

#[test]
fn range_without_classitem_emits_false() {
    let w = resolve_when(
        Some(ParsedExpression::Range {
            lo: Literal::Int(2),
            hi: Some(Literal::Int(12)),
        }),
        None,
        None,
        "layer",
        1,
    );
    // untranslatable -> when:false; the raw range is surfaced via warn!.
    assert_eq!(w.as_deref(), Some("false"));
}

#[test]
fn mixed_range_round_trips() {
    let w = resolve_when(
        Some(ParsedExpression::Range {
            lo: Literal::Int(0),
            hi: Some(Literal::Float(2.5)),
        }),
        Some("rtt"),
        None,
        "layer",
        1,
    );
    let s = w.unwrap();
    assert_eq!(s, "(rtt >= 0 AND rtt <= 2.5)");
    mars_expr::parse(&s).unwrap();
}

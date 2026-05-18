#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn quotes_plain() {
    assert_eq!(quote_ident("foo").unwrap(), "\"foo\"");
}

#[test]
fn doubles_embedded_quote() {
    assert_eq!(quote_ident("foo\"bar").unwrap(), "\"foo\"\"bar\"");
}

#[test]
fn rejects_dotted() {
    assert!(matches!(quote_ident("a.b"), Err(SourceError::Backend { .. })));
}

#[test]
fn rejects_nul() {
    assert!(matches!(quote_ident("a\0b"), Err(SourceError::Backend { .. })));
}

#[test]
fn rejects_empty() {
    assert!(matches!(quote_ident(""), Err(SourceError::Backend { .. })));
}

#[test]
fn split_from_dotted() {
    assert_eq!(split_from("public.roads"), ("public", "roads"));
    assert_eq!(
        split_from("geo.administrative.regions"),
        ("geo", "administrative.regions")
    );
}

#[test]
fn split_from_defaults_to_public() {
    assert_eq!(split_from("roads"), ("public", "roads"));
}

#[test]
fn render_from_target_table_form() {
    assert_eq!(render_from_target("public.roads").unwrap(), "\"public\".\"roads\"");
    assert_eq!(render_from_target("roads").unwrap(), "\"public\".\"roads\"");
}

#[test]
fn render_from_target_subquery_form() {
    let from = "(SELECT id, geom, name FROM public.points)";
    assert_eq!(
        render_from_target(from).unwrap(),
        "(SELECT id, geom, name FROM public.points) AS _mars_src"
    );
}

#[test]
fn render_from_target_rejects_malformed_table() {
    assert!(matches!(render_from_target(""), Err(SourceError::Backend { .. })));
}

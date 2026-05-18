#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use mars_expr::parse;
use mars_source::SourceCollectionId;
use mars_types::CrsCode;

fn binding(attrs: &[&str], id: &str) -> SourceBinding {
    SourceBinding::new(
        SourceCollectionId::new("c"),
        "public.t",
        "geom",
        id,
        attrs.iter().map(|s| (*s).to_string()).collect(),
        CrsCode::new("EPSG:25832"),
    )
    .unwrap()
}

#[test]
fn lowers_three_clause_filter() {
    let e = parse("ttype = 'forest' AND area >= 1000").unwrap();
    let b = binding(&["ttype", "area"], "gid");
    let (sql, params) = lower_to_sql(&e, &b, 1).unwrap();
    assert_eq!(sql, "\"ttype\" = $1 AND \"area\" >= $2");
    assert_eq!(params.len(), 2);
    assert!(matches!(&params[0], SqlParam::Text(s) if s == "forest"));
    assert!(matches!(&params[1], SqlParam::Int(1000)));
}

#[test]
fn rejects_unknown_ident() {
    let e = parse("evil = 1").unwrap();
    let b = binding(&["ttype"], "gid");
    let r = lower_to_sql(&e, &b, 1);
    assert!(matches!(r, Err(SourceError::UnknownIdent { name }) if name == "evil"));
}

#[test]
fn lowers_in_list() {
    let e = parse("kind IN ('a','b')").unwrap();
    let b = binding(&["kind"], "gid");
    let (sql, params) = lower_to_sql(&e, &b, 1).unwrap();
    assert_eq!(sql, "\"kind\" IN ($1, $2)");
    assert_eq!(params.len(), 2);
}

#[test]
fn lowers_like() {
    let e = parse("name LIKE 'foo%'").unwrap();
    let b = binding(&["name"], "gid");
    let (sql, params) = lower_to_sql(&e, &b, 1).unwrap();
    assert_eq!(sql, "\"name\" LIKE $1");
    assert!(matches!(&params[0], SqlParam::Text(s) if s == "foo%"));
}

#[test]
fn lowers_regex_case_sensitive() {
    let e = parse("name ~ '^foo'").unwrap();
    let b = binding(&["name"], "gid");
    let (sql, params) = lower_to_sql(&e, &b, 1).unwrap();
    assert_eq!(sql, "\"name\" ~ $1");
    assert!(matches!(&params[0], SqlParam::Text(s) if s == "^foo"));
}

#[test]
fn lowers_regex_case_insensitive() {
    let e = parse("name ~* 'bar'").unwrap();
    let b = binding(&["name"], "gid");
    let (sql, params) = lower_to_sql(&e, &b, 1).unwrap();
    assert_eq!(sql, "\"name\" ~* $1");
    assert!(matches!(&params[0], SqlParam::Text(s) if s == "bar"));
}

#[test]
fn lowers_regex_offsets_placeholder() {
    // confirm pattern uses caller-provided $N start, like LIKE / cmp do.
    let e = parse("name ~ 'x'").unwrap();
    let b = binding(&["name"], "gid");
    let (sql, _params) = lower_to_sql(&e, &b, 5).unwrap();
    assert_eq!(sql, "\"name\" ~ $5");
}

#[test]
fn lowers_is_not_null() {
    let e = parse("name IS NOT NULL").unwrap();
    let b = binding(&["name"], "gid");
    let (sql, params) = lower_to_sql(&e, &b, 1).unwrap();
    assert_eq!(sql, "\"name\" IS NOT NULL");
    assert!(params.is_empty());
}

#[test]
fn lowers_not_group() {
    let e = parse("NOT (a = 1)").unwrap();
    let b = binding(&["a"], "gid");
    let (sql, params) = lower_to_sql(&e, &b, 1).unwrap();
    assert_eq!(sql, "NOT (\"a\" = $1)");
    assert_eq!(params.len(), 1);
}

#[test]
fn id_field_in_allowlist() {
    let e = parse("gid = 7").unwrap();
    let b = binding(&[], "gid");
    let (sql, params) = lower_to_sql(&e, &b, 1).unwrap();
    assert_eq!(sql, "\"gid\" = $1");
    assert!(matches!(&params[0], SqlParam::Int(7)));
}

#[test]
fn null_literal_not_parameterised() {
    // NULL appears via IN list; eq/ne against NULL is rejected at parse.
    let e = parse("a IN (NULL, 1)").unwrap();
    let b = binding(&["a"], "gid");
    let (sql, params) = lower_to_sql(&e, &b, 1).unwrap();
    assert_eq!(sql, "\"a\" IN (NULL, $1)");
    assert_eq!(params.len(), 1);
}

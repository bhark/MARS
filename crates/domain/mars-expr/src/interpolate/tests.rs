#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::collections::BTreeMap;

struct Row(BTreeMap<&'static str, Literal>);
impl AttributeAccess for Row {
    fn get(&self, name: &str) -> Option<Literal> {
        self.0.get(name).cloned()
    }
}

fn row(pairs: &[(&'static str, Literal)]) -> Row {
    Row(pairs.iter().cloned().collect())
}

#[test]
fn empty_template_yields_no_segments() {
    let t = parse_template("").unwrap();
    assert!(t.segments.is_empty());
    assert_eq!(eval_template(&t, &row(&[])).unwrap(), "");
}

#[test]
fn pure_literal() {
    let t = parse_template("hello world").unwrap();
    assert_eq!(t.segments, vec![Segment::Literal("hello world".into())]);
    assert_eq!(eval_template(&t, &row(&[])).unwrap(), "hello world");
}

#[test]
fn pure_substitution() {
    let t = parse_template("{name}").unwrap();
    assert_eq!(t.segments, vec![Segment::Ident("name".into())]);
    let r = row(&[("name", Literal::String("alice".into()))]);
    assert_eq!(eval_template(&t, &r).unwrap(), "alice");
}

#[test]
fn mixed_segments() {
    let t = parse_template("[{kind}] {name} ({n})").unwrap();
    let r = row(&[
        ("kind", Literal::String("road".into())),
        ("name", Literal::String("Main".into())),
        ("n", Literal::Int(42)),
    ]);
    assert_eq!(eval_template(&t, &r).unwrap(), "[road] Main (42)");
}

#[test]
fn missing_attr_renders_empty() {
    let t = parse_template("name={name}, age={age}").unwrap();
    let r = row(&[("name", Literal::String("ada".into()))]);
    assert_eq!(eval_template(&t, &r).unwrap(), "name=ada, age=");
}

#[test]
fn null_attr_renders_empty() {
    let t = parse_template("{x}").unwrap();
    let r = row(&[("x", Literal::Null)]);
    assert_eq!(eval_template(&t, &r).unwrap(), "");
}

#[test]
fn rejects_unmatched_brace() {
    assert!(parse_template("{name").is_err());
}

#[test]
fn rejects_empty_placeholder() {
    assert!(parse_template("a{}b").is_err());
}

#[test]
fn rejects_invalid_ident() {
    assert!(parse_template("{1bad}").is_err());
    assert!(parse_template("{na me}").is_err());
    assert!(parse_template("{na-me}").is_err());
}

#[test]
fn rejects_stray_close() {
    assert!(parse_template("a}b").is_err());
}

#[test]
fn underscored_idents_ok() {
    let t = parse_template("{_x}{x_2}").unwrap();
    let r = row(&[
        ("_x", Literal::String("a".into())),
        ("x_2", Literal::String("b".into())),
    ]);
    assert_eq!(eval_template(&t, &r).unwrap(), "ab");
}

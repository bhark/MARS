#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn sample() -> Vec<(String, AttrValue)> {
    vec![
        ("n".into(), AttrValue::Null),
        ("b".into(), AttrValue::Bool(false)),
        ("i".into(), AttrValue::Int(7)),
        ("f".into(), AttrValue::Float(1.5)),
        ("s".into(), AttrValue::String("hi".into())),
    ]
}

#[test]
fn mapping_is_total_and_lossless() {
    let row = sample();
    let a = RowAttrs::new(&row);
    assert_eq!(a.get("n"), Some(Literal::Null));
    assert_eq!(a.get("b"), Some(Literal::Bool(false)));
    assert_eq!(a.get("i"), Some(Literal::Int(7)));
    assert_eq!(a.get("f"), Some(Literal::Float(1.5)));
    assert_eq!(a.get("s"), Some(Literal::String("hi".into())));
}

#[test]
fn unknown_ident_returns_none() {
    let row = sample();
    let a = RowAttrs::new(&row);
    assert!(a.get("missing").is_none());
}

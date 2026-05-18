#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::collections::HashMap;

struct Row(HashMap<String, Literal>);
impl AttributeAccess for Row {
    fn get(&self, name: &str) -> Option<Literal> {
        self.0.get(name).cloned()
    }
}
fn row(pairs: &[(&str, Literal)]) -> Row {
    Row(pairs.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect())
}

#[test]
fn bare_f32_roundtrips_yaml() {
    let yaml = "8.0\n";
    let n: NumericField = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(n, NumericField::Static(8.0));
    let out = serde_yaml_ng::to_string(&n).unwrap();
    assert_eq!(out.trim(), "8.0");
}

#[test]
fn bracket_string_form_roundtrips_yaml() {
    let yaml = "\"[bearing]\"\n";
    let n: NumericField = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(n, NumericField::Attribute("bearing".into()));
    let out = serde_yaml_ng::to_string(&n).unwrap();
    assert_eq!(out.trim(), "'[bearing]'");
}

#[test]
fn tagged_static_map_parses() {
    let yaml = "kind: static\nvalue: 7.5\n";
    let n: NumericField = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(n, NumericField::Static(7.5));
}

#[test]
fn tagged_attribute_map_parses() {
    let yaml = "kind: attribute\ncolumn: bearing\n";
    let n: NumericField = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(n, NumericField::Attribute("bearing".into()));
}

#[test]
fn bracket_form_rejects_non_ident() {
    let yaml = "\"[1bad]\"\n";
    assert!(serde_yaml_ng::from_str::<NumericField>(yaml).is_err());
    let yaml = "\"plain\"\n";
    assert!(serde_yaml_ng::from_str::<NumericField>(yaml).is_err());
    let yaml = "\"[]\"\n";
    assert!(serde_yaml_ng::from_str::<NumericField>(yaml).is_err());
}

#[test]
fn resolve_static_returns_value() {
    let n = NumericField::Static(3.0);
    assert_eq!(n.resolve(&row(&[])), Some(3.0));
}

#[test]
fn resolve_attribute_coerces_numerics() {
    let n = NumericField::Attribute("x".into());
    assert_eq!(n.resolve(&row(&[("x", Literal::Int(5))])), Some(5.0));
    assert_eq!(n.resolve(&row(&[("x", Literal::Float(2.5))])), Some(2.5));
    assert_eq!(n.resolve(&row(&[("x", Literal::String("7.25".into()))])), Some(7.25));
}

#[test]
fn resolve_attribute_missing_returns_none() {
    let n = NumericField::Attribute("missing".into());
    assert_eq!(n.resolve(&row(&[])), None);
}

#[test]
fn resolve_attribute_null_or_bool_returns_none() {
    let n = NumericField::Attribute("x".into());
    assert_eq!(n.resolve(&row(&[("x", Literal::Null)])), None);
    assert_eq!(n.resolve(&row(&[("x", Literal::Bool(true))])), None);
}

#[test]
fn resolve_attribute_unparseable_string_returns_none() {
    let n = NumericField::Attribute("x".into());
    assert_eq!(n.resolve(&row(&[("x", Literal::String("ten".into()))])), None);
}

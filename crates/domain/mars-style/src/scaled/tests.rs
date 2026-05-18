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
fn from_f32_yields_bare_form() {
    let s: ScaledSize = 8.0_f32.into();
    assert!((s.base_px - 8.0).abs() < f32::EPSILON);
    assert!(s.min_px.is_none() && s.max_px.is_none() && s.ref_denom.is_none() && s.attribute.is_none());
}

#[test]
fn resolve_without_ref_denom_is_clamp_only() {
    let s = ScaledSize {
        base_px: 10.0,
        min_px: Some(4.0),
        max_px: Some(20.0),
        ref_denom: None,
        attribute: None,
    };
    assert!((s.resolve(1) - 10.0).abs() < f32::EPSILON);
    assert!((s.resolve(1_000_000) - 10.0).abs() < f32::EPSILON);
}

#[test]
fn resolve_scales_linearly_by_ref_over_denom() {
    let s = ScaledSize {
        base_px: 8.0,
        min_px: None,
        max_px: None,
        ref_denom: Some(50_000),
        attribute: None,
    };
    assert!((s.resolve(50_000) - 8.0).abs() < f32::EPSILON);
    assert!((s.resolve(25_000) - 16.0).abs() < f32::EPSILON);
    assert!((s.resolve(100_000) - 4.0).abs() < f32::EPSILON);
}

#[test]
fn resolve_clamps_after_scaling() {
    let s = ScaledSize {
        base_px: 8.0,
        min_px: Some(2.0),
        max_px: Some(16.0),
        ref_denom: Some(50_000),
        attribute: None,
    };
    assert!((s.resolve(6_250) - 16.0).abs() < f32::EPSILON);
    assert!((s.resolve(400_000) - 2.0).abs() < f32::EPSILON);
}

#[test]
fn resolve_denom_zero_skips_scaling() {
    let s = ScaledSize {
        base_px: 8.0,
        min_px: Some(2.0),
        max_px: Some(16.0),
        ref_denom: Some(50_000),
        attribute: None,
    };
    assert!((s.resolve(0) - 8.0).abs() < f32::EPSILON);
}

#[test]
fn resolve_reads_attribute_when_set() {
    let s = ScaledSize {
        base_px: 1.0,
        min_px: None,
        max_px: None,
        ref_denom: None,
        attribute: Some("icon_size".into()),
    };
    let r = row(&[("icon_size", Literal::Float(12.0))]);
    assert!((s.resolve_with_attrs(0, &r) - 12.0).abs() < f32::EPSILON);
}

#[test]
fn resolve_attribute_falls_back_to_base_when_missing() {
    let s = ScaledSize {
        base_px: 9.0,
        min_px: None,
        max_px: None,
        ref_denom: None,
        attribute: Some("icon_size".into()),
    };
    assert!((s.resolve(0) - 9.0).abs() < f32::EPSILON);
}

#[test]
fn resolve_attribute_clamps_after_fetch() {
    let s = ScaledSize {
        base_px: 1.0,
        min_px: Some(4.0),
        max_px: Some(20.0),
        ref_denom: None,
        attribute: Some("size".into()),
    };
    let r = row(&[("size", Literal::Int(2))]);
    assert!((s.resolve_with_attrs(0, &r) - 4.0).abs() < f32::EPSILON);
    let r = row(&[("size", Literal::Int(99))]);
    assert!((s.resolve_with_attrs(0, &r) - 20.0).abs() < f32::EPSILON);
}

#[test]
fn bare_f32_roundtrips_via_yaml() {
    let yaml = "8.0\n";
    let s: ScaledSize = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(s, ScaledSize::from_px(8.0));
    let out = serde_yaml_ng::to_string(&s).unwrap();
    assert_eq!(out.trim(), "8.0");
}

#[test]
fn bracket_string_form_parses_to_attribute() {
    let yaml = "\"[icon_size]\"\n";
    let s: ScaledSize = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(s.attribute.as_deref(), Some("icon_size"));
}

#[test]
fn tagged_map_roundtrips_via_yaml() {
    let yaml = "base_px: 8.0\nmin_px: 2.0\nmax_px: 16.0\nref_denom: 50000\n";
    let s: ScaledSize = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(s.base_px, 8.0);
    assert_eq!(s.min_px, Some(2.0));
    assert_eq!(s.max_px, Some(16.0));
    assert_eq!(s.ref_denom, Some(50_000));
    let out = serde_yaml_ng::to_string(&s).unwrap();
    let back: ScaledSize = serde_yaml_ng::from_str(&out).unwrap();
    assert_eq!(back, s);
}

#[test]
fn tagged_map_drops_unset_fields_on_serialise() {
    let s = ScaledSize {
        base_px: 8.0,
        min_px: Some(2.0),
        max_px: None,
        ref_denom: None,
        attribute: None,
    };
    let out = serde_yaml_ng::to_string(&s).unwrap();
    assert!(out.contains("base_px: 8"));
    assert!(out.contains("min_px: 2"));
    assert!(!out.contains("max_px"));
    assert!(!out.contains("ref_denom"));
    assert!(!out.contains("attribute"));
}

#[test]
fn bare_int_yaml_parses() {
    let s: ScaledSize = serde_yaml_ng::from_str("8").unwrap();
    assert_eq!(s, ScaledSize::from_px(8.0));
}

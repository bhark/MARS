#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn newtype_serde_is_transparent() {
    let l = LayerId::new("parcels");
    let s = serde_json::to_string(&l).unwrap();
    assert_eq!(s, "\"parcels\"");
    let back: LayerId = serde_json::from_str(&s).unwrap();
    assert_eq!(back, l);
}

#[test]
fn binding_id_try_new_accepts_safe_segments() {
    assert!(BindingId::try_new("buildings").is_ok());
    assert!(BindingId::try_new("parcels-2024").is_ok());
    assert!(BindingId::try_new("plots_v3").is_ok());
}

#[test]
fn binding_id_try_new_rejects_unsafe() {
    assert!(BindingId::try_new("").is_err(), "empty");
    assert!(BindingId::try_new("foo/bar").is_err(), "slash");
    assert!(BindingId::try_new("foo\\bar").is_err(), "backslash");
    assert!(BindingId::try_new("a\0b").is_err(), "null");
    assert!(BindingId::try_new("..").is_err(), "dotdot");
    assert!(BindingId::try_new(".").is_err(), "dot");
    let big = "x".repeat(129);
    assert!(BindingId::try_new(big).is_err(), "too long");
}

#[test]
fn binding_id_serde_is_transparent() {
    let id = BindingId::try_new("buildings").unwrap();
    let s = serde_json::to_string(&id).unwrap();
    assert_eq!(s, "\"buildings\"");
    let back: BindingId = serde_json::from_str(&s).unwrap();
    assert_eq!(back, id);
}

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn img(byte: u8) -> Arc<DecodedImage> {
    Arc::new(DecodedImage {
        width: 1,
        height: 1,
        rgba: Arc::new(vec![byte, byte, byte, 255]),
    })
}

#[test]
fn empty_registry_returns_none() {
    let reg = MutableImageRegistry::new();
    assert!(reg.is_empty());
    assert!(reg.get("brick").is_none());
}

#[test]
fn set_then_get_returns_entry() {
    let reg = MutableImageRegistry::new();
    let mut map = HashMap::new();
    map.insert("brick".to_string(), img(1));
    reg.set(map);
    assert_eq!(reg.len(), 1);
    let got = reg.get("brick").expect("present");
    assert_eq!(got.rgba.as_slice(), &[1, 1, 1, 255]);
}

#[test]
fn second_set_replaces_prior() {
    let reg = MutableImageRegistry::new();
    let mut a = HashMap::new();
    a.insert("brick".into(), img(1));
    reg.set(a);
    let mut b = HashMap::new();
    b.insert("stone".into(), img(2));
    reg.set(b);
    assert!(reg.get("brick").is_none());
    assert_eq!(reg.get("stone").expect("stone").rgba.as_slice(), &[2, 2, 2, 255]);
}

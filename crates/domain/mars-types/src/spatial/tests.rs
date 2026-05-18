#![allow(clippy::unwrap_used)]

use super::*;

fn sample_page_key() -> PageKey {
    PageKey {
        binding_id: BindingId::try_new("buildings").unwrap(),
        level: DecimationLevel::new(2),
        page_id: PageId::new(0xdead_beef),
    }
}

#[test]
fn page_id_display_is_zero_padded_hex() {
    assert_eq!(PageId::new(0).to_string(), "0000000000000000");
    assert_eq!(PageId::new(0xdead_beef).to_string(), "00000000deadbeef");
    assert_eq!(PageId::new(u64::MAX).to_string(), "ffffffffffffffff");
}

#[test]
fn page_id_serde_is_transparent() {
    let p = PageId::new(42);
    let s = serde_json::to_string(&p).unwrap();
    assert_eq!(s, "42");
    let back: PageId = serde_json::from_str(&s).unwrap();
    assert_eq!(back, p);
}

#[test]
fn decimation_level_ordering() {
    assert!(DecimationLevel::new(0) < DecimationLevel::new(1));
    assert!(DecimationLevel::new(4) > DecimationLevel::new(2));
    assert_eq!(DecimationLevel::new(3).get(), 3);
    assert_eq!(DecimationLevel::new(7).to_string(), "7");
}

#[test]
fn hilbert_key_ordering_and_bounds() {
    assert!(HilbertKey::min() < HilbertKey::max());
    assert_eq!(HilbertKey::min().get(), 0);
    assert_eq!(HilbertKey::max().get(), u64::MAX);
    let k = HilbertKey::new(0xcafe_babe);
    assert_eq!(k.to_string(), "00000000cafebabe");
}

#[test]
fn hilbert_key_serde_is_transparent() {
    let k = HilbertKey::new(1234);
    let s = serde_json::to_string(&k).unwrap();
    assert_eq!(s, "1234");
    let back: HilbertKey = serde_json::from_str(&s).unwrap();
    assert_eq!(back, k);
}

#[test]
fn page_key_object_key_shape() {
    let pk = sample_page_key();
    let hash = ContentHash([0xab; 32]);
    let key = pk.object_key(&hash).unwrap();
    assert_eq!(
        key.as_str(),
        "bnd/buildings/L2/p00000000deadbeef/abababababababababababababababababababababababababababababababab.mars"
    );
}

#[test]
fn page_key_object_key_rejects_unsafe_binding() {
    // BindingId::try_new is the trust boundary; defense in depth here lets
    // us catch a hand-constructed BindingId that bypassed validation.
    let pk = PageKey {
        binding_id: BindingId::new("foo/bar"),
        level: DecimationLevel::new(0),
        page_id: PageId::new(0),
    };
    let hash = ContentHash::zero();
    assert!(pk.object_key(&hash).is_err());
}

#[test]
fn page_key_serde_roundtrip() {
    let pk = sample_page_key();
    let s = serde_json::to_string(&pk).unwrap();
    let back: PageKey = serde_json::from_str(&s).unwrap();
    assert_eq!(pk, back);
}

#[test]
fn layer_sidecar_entry_object_key_shapes() {
    let pk = sample_page_key();
    let class = LayerSidecarEntry {
        layer_id: LayerId::new("roads"),
        page_key: pk.clone(),
        content_hash: ContentHash([0x11; 32]),
        size_bytes: 1024,
        kind: LayerSidecarKind::Class,
    };
    assert!(
        class
            .object_key()
            .unwrap()
            .as_str()
            .starts_with("cls/roads/buildings/L2/p00000000deadbeef/")
    );

    let label = LayerSidecarEntry {
        kind: LayerSidecarKind::Label,
        ..class
    };
    assert!(
        label
            .object_key()
            .unwrap()
            .as_str()
            .starts_with("lbl/roads/buildings/L2/p00000000deadbeef/")
    );
}

#[test]
fn layer_sidecar_entry_rejects_unsafe_segments() {
    let pk = sample_page_key();
    let bad = LayerSidecarEntry {
        layer_id: LayerId::new(".."),
        page_key: pk,
        content_hash: ContentHash::zero(),
        size_bytes: 0,
        kind: LayerSidecarKind::Class,
    };
    assert!(bad.object_key().is_err());
}

#[test]
fn page_entry_serde_roundtrip() {
    let pe = PageEntry {
        key: sample_page_key(),
        content_hash: ContentHash([0x33; 32]),
        spatial_bbox: Bbox::new(0.0, 0.0, 100.0, 100.0),
        hilbert_range: (HilbertKey::new(1), HilbertKey::new(2_000)),
        feature_count: 40_000,
        size_bytes: 5 * 1024 * 1024,
    };
    let s = serde_json::to_string(&pe).unwrap();
    let back: PageEntry = serde_json::from_str(&s).unwrap();
    assert_eq!(pe, back);
}

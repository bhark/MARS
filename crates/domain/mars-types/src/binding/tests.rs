#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn level_metadata_serde_roundtrip() {
    let lm = LevelMetadata {
        level: DecimationLevel::new(1),
        vertex_tolerance_m: 0.5,
        geometry_min_size_m: 2.0,
        label_min_priority: 5,
        page_count: 12,
        hilbert_range_table: vec![
            (HilbertKey::new(0), HilbertKey::new(100), PageId::new(0)),
            (HilbertKey::new(101), HilbertKey::new(500), PageId::new(1)),
        ],
    };
    let s = serde_json::to_string(&lm).unwrap();
    let back: LevelMetadata = serde_json::from_str(&s).unwrap();
    assert_eq!(lm, back);
}

#[test]
fn binding_metadata_serde_roundtrip() {
    let bm = BindingMetadata {
        binding_id: BindingId::try_new("buildings").unwrap(),
        source_table: "public.buildings".to_owned(),
        native_crs: CrsCode::new("EPSG:25832"),
        feature_count_total: 5_000_000,
        combined_bbox: Bbox::new(-10.0, -10.0, 10.0, 10.0),
        levels: vec![],
        page_membership_sidecar: None,
        cycles_since_reconcile: 7,
        last_reconcile_at: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)),
    };
    let s = serde_json::to_string(&bm).unwrap();
    let back: BindingMetadata = serde_json::from_str(&s).unwrap();
    assert_eq!(bm, back);
}

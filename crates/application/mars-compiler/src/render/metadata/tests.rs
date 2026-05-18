#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use mars_types::{Bbox, BindingId, DecimationLevel, HilbertKey, PageEntry, PageId, PageKey};

fn page_entry(binding: &str, level: u8, page_id: u64, lo: u64, hi: u64) -> PageEntry {
    PageEntry {
        key: PageKey {
            binding_id: BindingId::try_new(binding).unwrap(),
            level: DecimationLevel::new(level),
            page_id: PageId::new(page_id),
        },
        content_hash: mars_types::ContentHash::zero(),
        spatial_bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
        hilbert_range: (HilbertKey::new(lo), HilbertKey::new(hi)),
        feature_count: 0,
        size_bytes: 0,
    }
}

#[test]
fn recompute_level_metadata_orders_ranges_and_counts_pages() {
    let prior = LevelMetadata {
        level: DecimationLevel::new(0),
        vertex_tolerance_m: 1.0,
        geometry_min_size_m: 0.0,
        label_min_priority: 0,
        page_count: 0,
        hilbert_range_table: vec![],
    };
    let pages = vec![
        page_entry("roads", 0, 0, 100, 200),
        page_entry("roads", 0, 1, 50, 75),
        page_entry("buildings", 0, 0, 0, 999),
        page_entry("roads", 1, 0, 0, 999),
    ];
    let updated = recompute_level_metadata(&prior, &pages, &BindingId::try_new("roads").unwrap());
    assert_eq!(updated.page_count, 2);
    assert_eq!(
        updated.hilbert_range_table,
        vec![
            (HilbertKey::new(50), HilbertKey::new(75), PageId::new(1)),
            (HilbertKey::new(100), HilbertKey::new(200), PageId::new(0)),
        ]
    );
}

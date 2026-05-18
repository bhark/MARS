#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use mars_types::{BindingId, ContentHash, DecimationLevel, HilbertKey, PageId};

fn page(id: u64, lo: u64, hi: u64, bbox: Bbox, size: u64) -> PageEntry {
    PageEntry {
        key: PageKey {
            binding_id: BindingId::try_new("b").unwrap(),
            level: DecimationLevel::new(0),
            page_id: PageId::new(id),
        },
        content_hash: ContentHash::zero(),
        spatial_bbox: bbox,
        hilbert_range: (HilbertKey::new(lo), HilbertKey::new(hi)),
        feature_count: 0,
        size_bytes: size,
    }
}

#[test]
fn empty_pages_yields_no_ops() {
    let combined = Bbox::new(0.0, 0.0, 100.0, 100.0);
    assert!(rebalance_candidates(combined, &[], 1000).is_empty());
}

#[test]
fn oversize_page_splits_into_proportional_count() {
    let combined = Bbox::new(0.0, 0.0, 100.0, 100.0);
    let target = 1_000;
    // 2.5x target -> ceil(2.5) = 3
    let p = page(0, 0, u64::MAX, combined, (2.5 * target as f64) as u64);
    let ops = rebalance_candidates(combined, std::slice::from_ref(&p), target);
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], RebalanceOp::Split { ref page, into: 3 } if page == &p.key));
}

/// half the hilbert key space.
const HALF: u64 = u64::MAX / 2;

#[test]
fn undersize_pair_merges() {
    let combined = Bbox::new(0.0, 0.0, 100.0, 100.0);
    // half-bbox per page so dilation stays in band even though sizes don't.
    let left_bbox = Bbox::new(0.0, 0.0, 50.0, 100.0);
    let right_bbox = Bbox::new(50.0, 0.0, 100.0, 100.0);
    let target = 1_000;
    let pages = vec![
        page(0, 0, HALF, left_bbox, 200),
        page(1, HALF + 1, u64::MAX, right_bbox, 200),
    ];
    let ops = rebalance_candidates(combined, &pages, target);
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        RebalanceOp::Merge { left, right } => {
            assert_eq!(left.page_id.get(), 0);
            assert_eq!(right.page_id.get(), 1);
        }
        other => panic!("expected merge, got {other:?}"),
    }
}

#[test]
fn in_band_pages_yield_no_ops() {
    let combined = Bbox::new(0.0, 0.0, 100.0, 100.0);
    let left_bbox = Bbox::new(0.0, 0.0, 50.0, 100.0);
    let right_bbox = Bbox::new(50.0, 0.0, 100.0, 100.0);
    let pages = vec![
        page(0, 0, HALF, left_bbox, 1_000),
        page(1, HALF + 1, u64::MAX, right_bbox, 1_100),
    ];
    assert!(rebalance_candidates(combined, &pages, 1_000).is_empty());
}

#[test]
fn split_takes_precedence_over_merge_neighbour() {
    let combined = Bbox::new(0.0, 0.0, 100.0, 100.0);
    let left_bbox = Bbox::new(0.0, 0.0, 50.0, 100.0);
    let right_bbox = Bbox::new(50.0, 0.0, 100.0, 100.0);
    let pages = vec![
        page(0, 0, HALF, left_bbox, 200),               // would merge ...
        page(1, HALF + 1, u64::MAX, right_bbox, 5_000), // ... but next is split
    ];
    let ops = rebalance_candidates(combined, &pages, 1_000);
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], RebalanceOp::Split { .. }));
}

#[test]
fn dilation_forces_split_even_in_size_band() {
    // page covers a tiny hilbert span but its bbox spans the full combined
    // bbox -> dilation > 4. size in band so only the dilation rule fires.
    // span = 1 key -> cell_area = combined_area / 2^32, ratio ~= 2^32.
    let combined = Bbox::new(0.0, 0.0, 100.0, 100.0);
    let p = page(0, 0, 0, combined, 1_000);
    let ops = rebalance_candidates(combined, std::slice::from_ref(&p), 1_000);
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], RebalanceOp::Split { ref page, into: 2 } if page == &p.key));
}

#[test]
fn oversize_split_with_realistic_full_span_page() {
    // single page covering the full key space; bbox = combined_bbox.
    // span/2^32 = 2^32 -> cell_area = 2^32 * combined_area >> page_area.
    // dilation < 1, but size triggers the split.
    let combined = Bbox::new(0.0, 0.0, 100.0, 100.0);
    let p = page(0, 0, u64::MAX, combined, 4_000);
    let ops = rebalance_candidates(combined, std::slice::from_ref(&p), 1_000);
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        RebalanceOp::Split { page, into } => {
            assert_eq!(page, &p.key);
            assert_eq!(*into, 4);
        }
        other => panic!("expected split, got {other:?}"),
    }
}

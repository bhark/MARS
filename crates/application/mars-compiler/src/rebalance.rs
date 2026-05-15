//! opportunistic page split/merge analyser.
//!
//! pure planner: given the page list of a single (binding × level) and the
//! target page size, returns the list of [`RebalanceOp`]s that bring the
//! distribution back inside `[SIZE_LO_FACTOR, SIZE_HI_FACTOR] × target` and
//! prevents bbox dilation from drifting past `BBOX_DILATION_FACTOR`.
//!
//! the executor lives in [`crate::render`] -- it reuses
//! `flush_page` / `emit_layer_sidecars` so split/merge results commit through
//! the same [`crate::render::RebuildOutcome`] shape as a regular cycle.
//!
//! Opportunistic, not tightly tuned. the goal is to keep page sizes
//! within an order of magnitude of target without the heuristics ever
//! becoming load-bearing for cycle latency.

use mars_types::{Bbox, PageEntry, PageKey};

/// pages below `SIZE_LO_FACTOR * target_bytes` qualify for merge with a
/// neighbour; pages above `SIZE_HI_FACTOR * target_bytes` qualify for split.
pub const SIZE_LO_FACTOR: f64 = 0.5;
pub const SIZE_HI_FACTOR: f64 = 1.5;
/// dilation = page_bbox_area / expected_hilbert_cell_area; above this ratio
/// a split is forced even if size is in band.
pub const BBOX_DILATION_FACTOR: f64 = 4.0;

/// per-dimension cell scale used as a proxy for "expected hilbert-cell area".
/// `cell_area ≈ (hi - lo) / 2^32 * combined_bbox_area`.
const HILBERT_CELL_DENOM: f64 = 4_294_967_296.0; // 2^32

/// one rebalance action against a (binding × level).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebalanceOp {
    /// split `page` into `into` sub-pages along the hilbert axis. `into >= 2`.
    Split {
        /// page being split
        page: PageKey,
        /// number of sub-pages to produce
        into: u32,
    },
    /// merge two adjacent pages into one. `left` and `right` must be adjacent
    /// in the level's hilbert range table.
    Merge {
        /// left page (smaller hilbert range)
        left: PageKey,
        /// right page (larger hilbert range)
        right: PageKey,
    },
}

/// identify rebalance candidates within one level. `pages` is the slice that
/// belongs to this level, in level-local order (sorted by `hilbert_range.0`).
/// `combined_bbox` is the binding-level hilbert-key basis; it is the same
/// for every level of a binding. the analyser is pure: it never reads the
/// source or the object store.
#[must_use]
pub fn rebalance_candidates(combined_bbox: Bbox, pages: &[PageEntry], target_bytes: u64) -> Vec<RebalanceOp> {
    if target_bytes == 0 || pages.is_empty() {
        return Vec::new();
    }
    let target = target_bytes as f64;
    let combined_area = bbox_area(combined_bbox);

    let mut ops: Vec<RebalanceOp> = Vec::new();
    let mut consumed = vec![false; pages.len()];

    // splits first; a split candidate is never also merged in the same pass.
    for (idx, page) in pages.iter().enumerate() {
        let size = page.size_bytes as f64;
        let size_split = size > SIZE_HI_FACTOR * target;
        let dilation_split = combined_area > 0.0 && bbox_dilation(page, combined_area) > BBOX_DILATION_FACTOR;
        if size_split || dilation_split {
            let into = if size_split {
                let ratio = (size / target).ceil() as u32;
                ratio.max(2)
            } else {
                2
            };
            ops.push(RebalanceOp::Split {
                page: page.key.clone(),
                into,
            });
            consumed[idx] = true;
        }
    }

    // merges: greedy left-to-right pass over adjacent pairs whose combined
    // size still fits within `SIZE_HI_FACTOR * target`.
    let mut i = 0;
    while i + 1 < pages.len() {
        if consumed[i] || consumed[i + 1] {
            i += 1;
            continue;
        }
        let lhs = pages[i].size_bytes as f64;
        let rhs = pages[i + 1].size_bytes as f64;
        if lhs < SIZE_LO_FACTOR * target && rhs < SIZE_LO_FACTOR * target && (lhs + rhs) <= SIZE_HI_FACTOR * target {
            ops.push(RebalanceOp::Merge {
                left: pages[i].key.clone(),
                right: pages[i + 1].key.clone(),
            });
            consumed[i] = true;
            consumed[i + 1] = true;
            i += 2;
        } else {
            i += 1;
        }
    }

    ops
}

fn bbox_area(b: Bbox) -> f64 {
    (b.max_x - b.min_x).max(0.0) * (b.max_y - b.min_y).max(0.0)
}

fn bbox_dilation(page: &PageEntry, combined_area: f64) -> f64 {
    let page_area = bbox_area(page.spatial_bbox);
    let (lo, hi) = page.hilbert_range;
    let span = (hi.get().saturating_sub(lo.get()) as f64) + 1.0;
    let cell_area = combined_area * (span / HILBERT_CELL_DENOM);
    if cell_area <= 0.0 {
        return 0.0;
    }
    page_area / cell_area
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
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
}

//! cycle stage 3b: dirty-set admission control.
//!
//! caps the per-binding incremental dirty-page count. when a binding
//! exceeds the configured ceiling - e.g. WAL replay storm, post-outage
//! backlog - we escalate it to a single truncate-class rebuild instead of
//! N per-page rebuilds. same end state, bounded work, observable via the
//! `mars_compiler_binding_truncate_total{reason="dirty_ceiling"}` counter.
//!
//! runs between ingest and rebuild so the per-binding `BindingDirty` is
//! still mutable, and BEFORE any other escalation pass (issue 1's
//! missing-page policy) so all escalations agree on the binding's final
//! state.

use mars_observability::{Metrics, truncate_reason};

use crate::incremental::DirtyPages;

/// scan `dirty.per_binding` and mark any binding whose incremental
/// dirty-page count exceeds `ceiling` as truncated. `None` disables the
/// ceiling. already-truncated bindings are skipped (the cost cap is N/A).
pub(crate) fn enforce_ceiling(dirty: &mut DirtyPages, ceiling: Option<usize>, metrics: &Metrics) {
    let Some(ceiling) = ceiling else { return };
    for (binding_id, bd) in dirty.per_binding.iter_mut() {
        if bd.truncated {
            continue;
        }
        let total: usize = bd.per_level.values().map(|s| s.len()).sum();
        if total > ceiling {
            tracing::warn!(
                binding = binding_id.as_str(),
                dirty_page_count = total,
                ceiling,
                "incremental dirty-page ceiling exceeded; escalating to truncate"
            );
            metrics.inc_compiler_binding_truncate(binding_id.as_str(), truncate_reason::DIRTY_CEILING);
            bd.truncated = true;
            bd.per_level.clear();
            bd.observed.clear();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    use mars_types::{BindingId, DecimationLevel, PageId};

    use crate::incremental::{BindingDirty, DirtyPages};

    fn dirty_pages(per_binding: Vec<(&str, BindingDirty)>) -> DirtyPages {
        let mut out = DirtyPages::default();
        for (id, bd) in per_binding {
            out.per_binding.insert(BindingId::try_new(id).unwrap(), bd);
        }
        out
    }

    fn binding_dirty_with_pages(level_pages: usize) -> BindingDirty {
        let mut per_level: BTreeMap<DecimationLevel, BTreeSet<PageId>> = BTreeMap::new();
        per_level.insert(
            DecimationLevel::new(0),
            (0..level_pages as u64).map(PageId::new).collect(),
        );
        BindingDirty {
            truncated: false,
            per_level,
            observed: BTreeSet::new(),
        }
    }

    #[test]
    fn under_ceiling_left_alone() {
        let metrics = Metrics::new().unwrap();
        let mut dirty = dirty_pages(vec![("roads", binding_dirty_with_pages(5))]);
        enforce_ceiling(&mut dirty, Some(10), &metrics);
        let bd = dirty.per_binding.get(&BindingId::try_new("roads").unwrap()).unwrap();
        assert!(!bd.truncated);
        assert_eq!(bd.per_level.len(), 1);
    }

    #[test]
    fn over_ceiling_escalates_to_truncate() {
        let metrics = Metrics::new().unwrap();
        let mut dirty = dirty_pages(vec![("roads", binding_dirty_with_pages(11))]);
        enforce_ceiling(&mut dirty, Some(10), &metrics);
        let bd = dirty.per_binding.get(&BindingId::try_new("roads").unwrap()).unwrap();
        assert!(bd.truncated);
        assert!(bd.per_level.is_empty());
        assert!(bd.observed.is_empty());
    }

    #[test]
    fn one_binding_over_one_under_isolated() {
        let metrics = Metrics::new().unwrap();
        let mut dirty = dirty_pages(vec![
            ("roads", binding_dirty_with_pages(11)),
            ("buildings", binding_dirty_with_pages(5)),
        ]);
        enforce_ceiling(&mut dirty, Some(10), &metrics);
        assert!(
            dirty
                .per_binding
                .get(&BindingId::try_new("roads").unwrap())
                .unwrap()
                .truncated
        );
        assert!(
            !dirty
                .per_binding
                .get(&BindingId::try_new("buildings").unwrap())
                .unwrap()
                .truncated
        );
    }

    #[test]
    fn already_truncated_skipped() {
        let metrics = Metrics::new().unwrap();
        let mut bd = binding_dirty_with_pages(11);
        bd.truncated = true;
        // pre-truncated bindings shouldn't fire the ceiling-driven metric.
        // (logic-wise we just keep `truncated=true` and never mutate.)
        let mut dirty = dirty_pages(vec![("roads", bd)]);
        enforce_ceiling(&mut dirty, Some(10), &metrics);
        let post = dirty.per_binding.get(&BindingId::try_new("roads").unwrap()).unwrap();
        assert!(post.truncated);
        assert_eq!(post.per_level.len(), 1, "we don't clobber prior truncation state");
    }

    #[test]
    fn none_ceiling_is_a_noop() {
        let metrics = Metrics::new().unwrap();
        let mut dirty = dirty_pages(vec![("roads", binding_dirty_with_pages(1_000))]);
        enforce_ceiling(&mut dirty, None, &metrics);
        let bd = dirty.per_binding.get(&BindingId::try_new("roads").unwrap()).unwrap();
        assert!(!bd.truncated);
    }
}

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
mod tests;

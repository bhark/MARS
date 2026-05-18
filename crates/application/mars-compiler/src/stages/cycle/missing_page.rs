//! cycle stage 3c: react to `IncrementalWarning::MissingPage` per the
//! affected binding's [`MissingPagePolicy`].
//!
//! a `MissingPage` warning means a change event's hilbert key fell outside
//! every page range for the binding - typically because the feature's
//! centroid sits outside the bootstrap `combined_bbox`. silently logging
//! and continuing leaves the row missing from the artifact until the next
//! reconcile cycle (up to `reconcile_every_cycles`, ~2h with defaults).
//!
//! per-binding policy resolution:
//! - `Warn`: log only; the next reconcile pass repairs drift.
//! - `Truncate`: escalate the binding to a full truncate-class rebuild
//!   this cycle. re-derives `combined_bbox` and re-emits every page.
//! - `Fail`: return [`CompilerError::MissingPageEscalation`] so the cycle
//!   aborts and operator alarms fire.
//!
//! runs after [`super::admission::enforce_ceiling`]: the ceiling already
//! caps total work; the missing-page pass only ever escalates further
//! (warn -> truncate -> fail). always-bumps the
//! `mars_compiler_missing_page_total{binding}` counter so operators see
//! the rate regardless of the resolved policy.

use std::collections::BTreeMap;

use mars_config::MissingPagePolicy;
use mars_observability::{Metrics, truncate_reason};
use mars_types::BindingId;

use crate::CompilerError;
use crate::incremental::{DirtyPages, IncrementalWarning};
use crate::plan::BootstrapPlan;

pub(crate) fn apply(dirty: &mut DirtyPages, plan: &BootstrapPlan, metrics: &Metrics) -> Result<(), CompilerError> {
    // group MissingPage warnings by binding so each binding hits at most
    // one policy lookup + at most one escalation action.
    let mut per_binding: BTreeMap<BindingId, Vec<&IncrementalWarning>> = BTreeMap::new();
    for w in &dirty.warnings {
        if let IncrementalWarning::MissingPage { binding_id, .. } = w {
            per_binding.entry(binding_id.clone()).or_default().push(w);
            metrics.inc_compiler_missing_page(binding_id.as_str());
        }
    }

    for (binding_id, warnings) in per_binding {
        let policy = plan
            .bindings
            .iter()
            .find(|b| b.binding_id == binding_id)
            .map(|b| b.missing_page_policy)
            .unwrap_or_default();

        match policy {
            MissingPagePolicy::Warn => {
                // logged already at cycle entry; nothing more to do here.
            }
            MissingPagePolicy::Truncate => {
                let bd = dirty.per_binding.entry(binding_id.clone()).or_default();
                if !bd.truncated {
                    tracing::warn!(
                        binding = binding_id.as_str(),
                        events = warnings.len(),
                        "missing-page escalation: truncating binding to re-derive combined_bbox"
                    );
                    metrics.inc_compiler_binding_truncate(binding_id.as_str(), truncate_reason::MISSING_PAGE);
                    bd.truncated = true;
                    bd.per_level.clear();
                    bd.observed.clear();
                }
            }
            MissingPagePolicy::Fail => {
                // group is non-empty by construction (we only insert when we
                // observe a MissingPage variant). pattern-match defensively
                // so a future warning variant added to this group can't
                // smuggle past as a silent no-op.
                for w in warnings {
                    if let IncrementalWarning::MissingPage { level, key, .. } = w {
                        return Err(CompilerError::MissingPageEscalation {
                            binding: binding_id.as_str().to_string(),
                            level: level.get(),
                            key: key.get(),
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

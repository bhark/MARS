//! cycle stage 5: merge the rebuild outcome into the prior manifest with
//! the latest-batch `source_version` threaded through. also stamps each
//! binding's `cycles_since_reconcile` from the in-memory counter and
//! `last_reconcile_at` from the cadence stage's outcome so cadence state
//! survives leader handover.

use mars_types::Manifest;

use crate::Compiler;
use crate::render::RebuildOutcome;
use crate::stages::cycle::reconcile_cadence::ReconcileCadenceOutcome;
use crate::stages::shared::merge::merge_manifest;

pub(crate) fn run(
    c: &Compiler,
    prior: &Manifest,
    outcome: &RebuildOutcome,
    last_source_version: Option<String>,
    cadence: &ReconcileCadenceOutcome,
) -> Manifest {
    let next_version = prior.version + 1;
    let mut manifest = merge_manifest(prior, outcome, next_version, last_source_version);
    let refreshed: Vec<_> = outcome
        .refreshed_bindings
        .iter()
        .map(|b| b.binding_id.clone())
        .collect();
    stamp_reconcile_state(c, &mut manifest, &refreshed, cadence);
    manifest
}

pub(crate) fn stamp_reconcile_state(
    c: &Compiler,
    manifest: &mut Manifest,
    refreshed: &[mars_types::BindingId],
    cadence: &ReconcileCadenceOutcome,
) {
    let counters = c.cycle_counter.lock();
    for b in &mut manifest.bindings {
        let was_truncated = refreshed.iter().any(|id| id == &b.binding_id);
        // a truncate-via-cycle resyncs the binding from source; treat as
        // a fresh reconcile so the next leader sees the stamp.
        if was_truncated || cadence.reconciled.contains(&b.binding_id) {
            b.cycles_since_reconcile = 0;
            b.last_reconcile_at = Some(cadence.reconciled_at);
        } else {
            b.cycles_since_reconcile = counters.get(&b.binding_id).copied().unwrap_or(0);
            // last_reconcile_at preserved by merge_manifest (cloned from prior).
        }
    }
}

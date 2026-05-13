//! cycle stage 2: periodic per-binding reconciliation.
//!
//! advances each binding's cycle counter and, for any binding whose
//! counter hits its configured cadence, runs a reconciliation pass. the
//! counter mutation lives under one write lock; the reconciliation await
//! runs lock-free with the snapshot of due bindings taken inside the
//! critical section.
//!
//! takes `&Compiler` because `cycle_counter` lives on `Compiler` (its
//! reset-on-leader-handover semantics are scoped to the leader-lock
//! lifetime, not a per-call ctx). plan and sidecars are passed by
//! reference; once `CycleCtx` lands they will flow through it.

use std::collections::HashMap;

use mars_source::ChangeEvent;
use mars_types::BindingId;

use crate::plan::{BindingPlan, BootstrapPlan};
use crate::reconcile;
use crate::sidecar::SidecarReader;
use crate::{Compiler, CompilerError};

pub(crate) async fn run(
    c: &Compiler,
    plan: &BootstrapPlan,
    sidecars: &HashMap<BindingId, SidecarReader<'_>>,
) -> Result<Vec<ChangeEvent>, CompilerError> {
    // critical section: advance counters, snapshot due bindings, reset
    // their counters. no await held under the lock.
    let due: Vec<BindingPlan> = {
        let mut counters = c.cycle_counter.write().await;
        let mut due = Vec::new();
        for binding_plan in &plan.bindings {
            let counter = counters.entry(binding_plan.binding_id.clone()).or_insert(0);
            *counter = counter.saturating_add(1);
            if *counter >= binding_plan.reconcile_every_cycles {
                *counter = 0;
                due.push(binding_plan.clone());
            }
        }
        due
    };

    let mut events: Vec<ChangeEvent> = Vec::new();
    for binding_plan in &due {
        let Some(sc) = sidecars.get(&binding_plan.binding_id) else {
            continue;
        };
        let outcome = reconcile::reconcile_binding(&c.deps, binding_plan, sc).await?;
        for w in [
            ("missing_in_sidecar", outcome.report.missing_in_sidecar.len()),
            ("orphan_in_sidecar", outcome.report.orphan_in_sidecar.len()),
        ] {
            if w.1 > 0 {
                tracing::warn!(
                    binding = binding_plan.binding_id.as_str(),
                    kind = w.0,
                    count = w.1,
                    "page-membership sidecar drift repaired by reconciliation"
                );
            }
        }
        events.extend(outcome.synthetic_events);
    }
    Ok(events)
}

//! cycle stage 2: periodic per-binding reconciliation.
//!
//! advances each binding's cycle counter and, for any binding whose
//! counter hits its configured cadence (or whose wall-clock-since-last-
//! reconcile crosses `reconcile_max_age`), runs a reconciliation pass.
//! the counter mutation lives under one sync lock; the reconciliation
//! await runs lock-free with the snapshot of due bindings taken inside
//! the critical section.
//!
//! the in-memory counter is hydrated on first observation per binding
//! from `prior.bindings[*].cycles_since_reconcile`, so leader handover
//! and process restart do not reset cadence. the wall-clock floor
//! covers the gap when a never-reconciled binding has nothing to
//! restore the counter from.
//!
//! takes `&Compiler` because `cycle_counter` lives on `Compiler`; plan
//! + sidecars come from `&CycleCtx`.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

use mars_source::{BindingHealth, ChangeEvent, RebindReason};
use mars_types::{BindingId, BindingMetadata};

use crate::plan::BindingPlan;
use crate::reconcile;
use crate::sidecar::SidecarReader;
use crate::stages::ctx::CycleCtx;
use crate::{Compiler, CompilerError};

/// what the cadence stage hands off to the merge stage. `reconciled_at`
/// is captured once per cycle so every reconciled binding stamps the
/// same instant into its `last_reconcile_at`.
pub(crate) struct ReconcileCadenceOutcome {
    pub(crate) reconciled_at: SystemTime,
    pub(crate) reconciled: HashSet<BindingId>,
}

pub(crate) async fn run(
    c: &Compiler,
    ctx: &CycleCtx,
    sidecars: &HashMap<BindingId, SidecarReader<'_>>,
) -> Result<(Vec<ChangeEvent>, ReconcileCadenceOutcome), CompilerError> {
    let now = SystemTime::now();
    let max_age = c.config.compiler.reconcile_max_age_dur()?;

    // critical section: advance counters, snapshot due bindings, reset
    // their counters. no await held under the lock.
    let due: Vec<BindingPlan> = {
        let mut counters = c.cycle_counter.lock();
        let mut due = Vec::new();
        for binding_plan in &ctx.plan.bindings {
            let prior_meta = ctx
                .prior
                .bindings
                .iter()
                .find(|b| b.binding_id == binding_plan.binding_id);
            if step_counter(
                &mut counters,
                &binding_plan.binding_id,
                binding_plan.reconcile_every_cycles,
                prior_meta,
                max_age,
                now,
            ) {
                due.push(binding_plan.clone());
            }
        }
        due
    };

    // publication-membership probe: backstop for the "binding silently
    // dropped from the publication" case the in-band Relation messages
    // cannot deliver. one query per source covers all due bindings on it;
    // sources with no publication concept return Healthy via the default impl.
    let mut due_by_source: HashMap<mars_config::SourceId, Vec<mars_source::SourceCollectionId>> = HashMap::new();
    for b in &due {
        due_by_source
            .entry(b.source_id.clone())
            .or_default()
            .push(mars_source::SourceCollectionId::new(b.binding_id.as_str()));
    }
    let mut unpublished: HashSet<BindingId> = HashSet::new();
    for (source_id, ids) in &due_by_source {
        let Some(src) = c.deps.sources.get(source_id) else {
            continue;
        };
        for h in src.probe_binding_health(ids).await? {
            if let BindingHealth::Unpublished(id) = h
                && let Ok(b) = BindingId::try_new(id.as_str())
            {
                unpublished.insert(b);
            }
        }
    }

    let mut events: Vec<ChangeEvent> = Vec::new();
    let mut reconciled: HashSet<BindingId> = HashSet::new();
    for binding_plan in &due {
        if unpublished.contains(&binding_plan.binding_id) {
            // unpublished bindings get a Rebind { BindingUnpublished }
            // synthesised straight into the cycle's event stream; the
            // compiler degrades them via the failure-isolation path so
            // prior pages stay served.
            tracing::warn!(
                binding = binding_plan.binding_id.as_str(),
                "binding absent from publication; surfacing as Rebind"
            );
            events.push(ChangeEvent::Rebind {
                collection: mars_source::SourceCollectionId::new(binding_plan.binding_id.as_str()),
                reason: RebindReason::BindingUnpublished,
            });
            continue;
        }
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
        reconciled.insert(binding_plan.binding_id.clone());
    }
    Ok((
        events,
        ReconcileCadenceOutcome {
            reconciled_at: now,
            reconciled,
        },
    ))
}

/// advance the in-memory counter for one binding and decide whether the
/// binding is due for reconciliation. seeds the counter from prior on
/// first observation (so leader handover preserves cadence), resets it
/// to zero on hit. returns true if due.
fn step_counter(
    counters: &mut HashMap<BindingId, u32>,
    binding_id: &BindingId,
    max_cycles: u32,
    prior_meta: Option<&BindingMetadata>,
    max_age: Option<Duration>,
    now: SystemTime,
) -> bool {
    let counter = counters
        .entry(binding_id.clone())
        .or_insert_with(|| prior_meta.map(|b| b.cycles_since_reconcile).unwrap_or(0));
    *counter = counter.saturating_add(1);

    let force_by_age = max_age
        .and_then(|max| {
            let last = prior_meta.and_then(|b| b.last_reconcile_at)?;
            now.duration_since(last).ok().map(|elapsed| elapsed > max)
        })
        .unwrap_or(false);

    if *counter >= max_cycles || force_by_age {
        *counter = 0;
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests;

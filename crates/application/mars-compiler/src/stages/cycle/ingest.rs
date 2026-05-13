//! cycle stage 3: build an `IncrementalCycle`, ingest reconcile events
//! and every change-batch event, and return the dirty set + scalars the
//! downstream stages need.
//!
//! pure: no I/O, no metric emission. the orchestrator logs warnings and
//! advances metrics counters from [`IngestOutcome`].

use std::collections::HashMap;

use mars_source::{ChangeBatch, ChangeEvent};
use mars_types::{BindingId, LevelMetadata, Manifest};

use crate::CompilerError;
use crate::incremental::{DirtyPages, IncrementalCycle};
use crate::plan::BootstrapPlan;
use crate::sidecar::SidecarReader;

pub(crate) struct IngestOutcome {
    pub(crate) dirty: DirtyPages,
    pub(crate) last_source_version: Option<String>,
    pub(crate) event_count: u64,
}

pub(crate) fn run(
    plan: &BootstrapPlan,
    sidecars: &HashMap<BindingId, SidecarReader<'_>>,
    prior: &Manifest,
    reconcile_events: Vec<ChangeEvent>,
    batches: Vec<ChangeBatch>,
) -> Result<IngestOutcome, CompilerError> {
    let level_meta: HashMap<BindingId, Vec<LevelMetadata>> = prior
        .bindings
        .iter()
        .map(|b| (b.binding_id.clone(), b.levels.clone()))
        .collect();
    let mut cycle = IncrementalCycle::new(plan, sidecars, &level_meta);
    let mut last_source_version: Option<String> = prior.source_version.clone();
    let mut event_count: u64 = 0;
    for event in reconcile_events {
        cycle.ingest(event)?;
        event_count += 1;
    }
    for batch in batches {
        for event in batch.events {
            cycle.ingest(event)?;
            event_count += 1;
        }
        if let Some(v) = batch.source_version {
            last_source_version = Some(v);
        }
    }
    let dirty = cycle.finish();
    Ok(IngestOutcome {
        dirty,
        last_source_version,
        event_count,
    })
}

//! cycle stage 3: build an `IncrementalCycle`, ingest reconcile events
//! and every change-batch event, and return the dirty set + scalars the
//! downstream stages need.
//!
//! pure: no I/O, no metric emission. the orchestrator logs warnings and
//! advances metrics counters from [`IngestOutcome`].

use std::collections::HashMap;

use mars_source::{ChangeBatch, ChangeEvent};
use mars_types::{BindingId, BindingMetadata};

use crate::CompilerError;
use crate::incremental::{DirtyPages, IncrementalCycle};
use crate::sidecar::SidecarReader;
use crate::stages::ctx::CycleCtx;

pub(crate) struct IngestOutcome {
    pub(crate) dirty: DirtyPages,
    pub(crate) last_source_version: Option<String>,
    pub(crate) event_count: u64,
}

pub(crate) fn run(
    ctx: &CycleCtx,
    sidecars: &HashMap<BindingId, SidecarReader<'_>>,
    reconcile_events: Vec<ChangeEvent>,
    batches: Vec<ChangeBatch>,
) -> Result<IngestOutcome, CompilerError> {
    let binding_meta: HashMap<BindingId, BindingMetadata> = ctx
        .prior
        .bindings
        .iter()
        .map(|b| (b.binding_id.clone(), b.clone()))
        .collect();
    let mut cycle = IncrementalCycle::new(&ctx.plan, sidecars, &binding_meta);
    let mut last_source_version: Option<String> = ctx.prior.source_version.clone();
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

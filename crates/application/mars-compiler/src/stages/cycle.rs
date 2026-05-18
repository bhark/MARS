//! cycle pipeline orchestrator.
//!
//! sequence: plan -> reconcile_cadence -> ingest -> admission (dirty-page
//! ceiling) -> missing_page (policy escalation) -> (no-op bump if empty)
//! -> rebuild -> merge -> publish. each stage is one module below.

mod admission;
mod ingest;
mod merge;
mod missing_page;
mod plan;
mod rebuild;
mod reconcile_cadence;

use mars_source::ChangeBatch;
use tokio_util::sync::CancellationToken;

use crate::stages::shared::{noop_bump, publish};
use crate::{Compiler, CompilerError};

pub(crate) async fn run(
    c: &Compiler,
    batches: Vec<ChangeBatch>,
    shutdown: &CancellationToken,
) -> Result<u64, CompilerError> {
    let ctx = plan::build(c).await?;
    let sidecars = ctx.sidecars.readers()?;

    let (reconcile_events, cadence) = reconcile_cadence::run(c, &ctx, &sidecars).await?;
    let ingest::IngestOutcome {
        mut dirty,
        last_source_version,
        event_count,
    } = ingest::run(&ctx, &sidecars, reconcile_events, batches)?;

    // admission control: cap per-binding dirty pages by escalating offenders
    // to a single truncate-class rebuild. happens before any other dirty-set
    // mutation so downstream stages see one consistent state.
    admission::enforce_ceiling(
        &mut dirty,
        c.config.compiler.incremental_dirty_page_ceiling_per_binding,
        &c.deps.metrics,
    );

    for w in &dirty.warnings {
        tracing::warn!(?w, "incremental cycle warning");
    }
    // missing-page policy escalation. may return CompilerError::MissingPageEscalation
    // under MissingPagePolicy::Fail; otherwise mutates `dirty` in place
    // (Truncate path) or is a no-op (Warn path).
    missing_page::apply(&mut dirty, &ctx.plan, &c.deps.metrics)?;
    c.deps.metrics.inc_compiler_dirty_cells(
        dirty
            .per_binding
            .values()
            .map(|d| d.per_level.values().map(|s| s.len() as u64).sum::<u64>())
            .sum::<u64>(),
    );
    if event_count > 0 {
        for _ in 0..event_count {
            c.deps.metrics.inc_compiler_change_events();
        }
    }
    if dirty.per_binding.is_empty() {
        // no work; publish a no-op version bump so downstream cursors
        // advance even on empty windows. still flush the counter / reconcile
        // state into the new manifest so a later leader picks up where we
        // left off.
        let mut next = noop_bump::build(ctx.prior, last_source_version);
        merge::stamp_reconcile_state(c, &mut next, &[], &cadence);
        return publish::with_retry(c.deps.manifest.as_ref(), &next, &c.deps.metrics, shutdown).await;
    }

    let outcome = rebuild::run(&c.deps, &c.deps.metrics, &ctx, &sidecars, dirty).await?;
    let manifest = merge::run(c, &ctx.prior, &outcome, last_source_version, &cadence);
    publish::with_retry(c.deps.manifest.as_ref(), &manifest, &c.deps.metrics, shutdown).await
}

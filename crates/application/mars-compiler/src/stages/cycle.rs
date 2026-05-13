//! cycle pipeline orchestrator.
//!
//! sequence: plan -> reconcile_cadence -> ingest -> (no-op bump if empty)
//! -> rebuild -> merge -> publish. each stage is one module below.

mod ingest;
mod merge;
mod plan;
mod rebuild;
mod reconcile_cadence;

use mars_source::ChangeBatch;
use tokio_util::sync::CancellationToken;

use crate::stages::shared::noop_bump;
use crate::{Compiler, CompilerError};

pub(crate) async fn run(
    c: &Compiler,
    batches: Vec<ChangeBatch>,
    shutdown: &CancellationToken,
) -> Result<u64, CompilerError> {
    let ctx = plan::build(c).await?;
    let sidecars = ctx.sidecars.readers()?;

    let reconcile_events = reconcile_cadence::run(c, &ctx, &sidecars).await?;
    let ingest::IngestOutcome {
        dirty,
        last_source_version,
        event_count,
    } = ingest::run(&ctx, &sidecars, reconcile_events, batches)?;

    for w in &dirty.warnings {
        tracing::warn!(?w, "incremental cycle warning");
    }
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
        // advance even on empty windows.
        let next = noop_bump::build(ctx.prior, last_source_version);
        return crate::publish_with_retry(c.deps.manifest.as_ref(), &next, &c.deps.metrics, shutdown).await;
    }

    let outcome = rebuild::run(&c.deps, &c.deps.metrics, &ctx, &sidecars, dirty).await?;
    let manifest = merge::run(&ctx.prior, &outcome, last_source_version);
    crate::publish_with_retry(c.deps.manifest.as_ref(), &manifest, &c.deps.metrics, shutdown).await
}

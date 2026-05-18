//! rebalance pipeline orchestrator.
//!
//! sequence: plan -> candidates -> (no-op bump if empty) -> sidecars ->
//! execute -> merge -> publish. each stage is one module under
//! `stages/rebalance/`.

mod candidates;
mod execute;
mod merge;
mod plan;

use tokio_util::sync::CancellationToken;

use crate::stages::shared::sidecars::OwnedSidecars;
use crate::stages::shared::{noop_bump, publish};
use crate::{Compiler, CompilerError};

pub(crate) async fn run(c: &Compiler) -> Result<u64, CompilerError> {
    let ctx = plan::build(c).await?;
    let ops = candidates::collect(&ctx);
    if ops.is_empty() {
        // already balanced; bump version so cursors advance.
        let sv = ctx.prior.source_version.clone();
        let next = noop_bump::build(ctx.prior, sv);
        return publish::with_retry(
            c.deps.manifest.as_ref(),
            &next,
            &c.deps.metrics,
            &CancellationToken::new(),
        )
        .await;
    }

    // mmap each binding's page-membership sidecar so the executor can
    // resolve feature-id sets per source page.
    let sidecars_owned = OwnedSidecars::fetch(c.deps.store.as_ref(), &ctx.prior.bindings).await?;
    let sidecars = sidecars_owned.readers()?;

    let outcome = execute::run(&c.deps, &ctx, ops, &sidecars).await?;
    let manifest = merge::run(&ctx.prior, &outcome);
    publish::with_retry(
        c.deps.manifest.as_ref(),
        &manifest,
        &c.deps.metrics,
        &CancellationToken::new(),
    )
    .await
}

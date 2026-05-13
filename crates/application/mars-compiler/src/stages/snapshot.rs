//! snapshot pipeline orchestrator.
//!
//! sequence: plan -> compile (unified pass-1 + pass-2 per binding) ->
//! publish. each stage is one module under `stages/snapshot/`.

mod compile;
mod plan;

use tokio_util::sync::CancellationToken;

use crate::{Compiler, CompilerError};

pub(crate) async fn run(c: &Compiler, shutdown: &CancellationToken) -> Result<u64, CompilerError> {
    let ctx = plan::build(c).await?;
    let manifest = compile::run(&c.deps, &ctx).await?;
    let v = crate::publish_with_retry(c.deps.manifest.as_ref(), &manifest, &c.deps.metrics, shutdown).await?;
    tracing::info!(
        version = v,
        bindings = manifest.bindings.len(),
        pages = manifest.pages.len(),
        "compiler: snapshot manifest published"
    );
    Ok(v)
}

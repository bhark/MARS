//! rebalance stage 1: build [`RebalanceCtx`] from the current manifest
//! and config.

use crate::stages::ctx::RebalanceCtx;
use crate::{Compiler, CompilerError};

pub(crate) async fn build(c: &Compiler) -> Result<RebalanceCtx, CompilerError> {
    let prior = c.deps.manifest.current().await?.ok_or(CompilerError::NoPriorManifest {
        context: "rebalance_locked",
    })?;
    let plan = crate::plan::build_bootstrap_plan(&c.config)?;
    let working_set_bytes = c.config.compiler.compile_page_working_set()?;
    Ok(RebalanceCtx {
        plan,
        prior,
        working_set_bytes,
    })
}

//! rebalance stage 3: execute the Split / Merge ops via
//! [`crate::render::execute_rebalance`].

use std::collections::HashMap;

use mars_types::BindingId;

use crate::rebalance::RebalanceOp;
use crate::render::{self, RebuildOutcome};
use crate::sidecar::SidecarReader;
use crate::stages::ctx::RebalanceCtx;
use crate::{CompilerError, Deps};

pub(crate) async fn run(
    deps: &Deps,
    ctx: &RebalanceCtx,
    ops: Vec<RebalanceOp>,
    sidecars: &HashMap<BindingId, SidecarReader<'_>>,
) -> Result<RebuildOutcome, CompilerError> {
    render::execute_rebalance(deps, &ctx.plan, &ctx.prior, sidecars, ops, ctx.working_set_bytes).await
}

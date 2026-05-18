//! snapshot stage 2: run the unified compile pipeline and log governor
//! observations.
//!
//! delegates to [`crate::snapshot_pipeline::run_snapshot_from_plan`]; the
//! same fn is re-exported at the crate root because external tests pin
//! `mars_compiler::run_snapshot_from_plan`.

use mars_types::Manifest;

use crate::snapshot_pipeline::run_snapshot_from_plan;
use crate::stages::ctx::SnapshotCtx;
use crate::stages::shared::governors;
use crate::{CompilerError, Deps};

pub(crate) async fn run(deps: &Deps, ctx: &SnapshotCtx) -> Result<Manifest, CompilerError> {
    let manifest = run_snapshot_from_plan(
        deps,
        &ctx.plan,
        ctx.service_name.clone(),
        ctx.next_version,
        ctx.knobs.working_set_bytes,
        ctx.knobs.plan_budget_bytes,
        ctx.knobs.in_flight_budget_bytes,
        ctx.knobs.binding_parallelism,
        &ctx.knobs.spill_dir,
        ctx.knobs.spill_open_file_limit,
        &ctx.mem_governor,
        &ctx.disk_governor,
    )
    .await?;
    governors::log_memory_observations("compile.snapshot.governor", &ctx.mem_governor);
    governors::log_disk_observations("compile.snapshot.disk_governor", &ctx.disk_governor);
    Ok(manifest)
}

//! cycle stage 4: rebuild dirty pages.
//!
//! invokes [`crate::render::rebuild_pages`] with the knobs + governors
//! held by [`CycleCtx`], times the call, and emits the duration metric
//! plus governor observations. spill-side admission flows through
//! `ctx.disk_governor`; `log_disk_observations` then reflects real
//! peak/wait values rather than the always-zero pre-admission state.

use std::collections::HashMap;

use mars_observability::Metrics;
use mars_types::BindingId;

use crate::incremental::DirtyPages;
use crate::render::{self, RebuildOutcome};
use crate::sidecar::SidecarReader;
use crate::stages::ctx::CycleCtx;
use crate::stages::shared::governors;
use crate::{CompilerError, Deps};

pub(crate) async fn run(
    deps: &Deps,
    metrics: &Metrics,
    ctx: &CycleCtx,
    sidecars: &HashMap<BindingId, SidecarReader<'_>>,
    dirty: DirtyPages,
) -> Result<RebuildOutcome, CompilerError> {
    let started = std::time::Instant::now();
    let outcome = render::rebuild_pages(
        deps,
        &ctx.plan,
        &ctx.prior,
        sidecars,
        dirty,
        ctx.knobs.working_set_bytes,
        ctx.knobs.plan_budget_bytes,
        ctx.knobs.in_flight_budget_bytes,
        &ctx.knobs.spill_dir,
        ctx.knobs.spill_open_file_limit,
        &ctx.mem_governor,
        &ctx.disk_governor,
        ctx.failure_policy,
    )
    .await?;
    metrics.observe_compiler_rebuild_duration(started.elapsed());
    governors::log_memory_observations("compile.cycle.governor", &ctx.mem_governor);
    governors::log_disk_observations("compile.cycle.disk_governor", &ctx.disk_governor);
    Ok(outcome)
}

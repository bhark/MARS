//! cycle stage 4: rebuild dirty pages.
//!
//! invokes [`crate::render::rebuild_pages`] with the knobs + governors
//! held by [`CycleCtx`], times the call, and emits the duration metric
//! plus governor observations.
//!
//! the disk governor is constructed (on `CycleCtx`) and observed here but
//! not yet plumbed into `render::rebuild_pages` itself (spill admission
//! is currently unbounded under disk pressure). this surfaces it through
//! the named ctx field rather than dropping it on the floor so a future
//! patch wiring it through `render::rebuild_pages` lands at one named
//! site. TODO(compile-disk-governor): pass &ctx.disk_governor into
//! render::rebuild_pages once the executor accepts it.

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
    )
    .await?;
    metrics.observe_compiler_rebuild_duration(started.elapsed());
    governors::log_memory_observations("compile.cycle.governor", &ctx.mem_governor);
    governors::log_disk_observations("compile.cycle.disk_governor", &ctx.disk_governor);
    Ok(outcome)
}

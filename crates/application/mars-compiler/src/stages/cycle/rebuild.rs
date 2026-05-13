//! cycle stage 4: rebuild dirty pages.
//!
//! resolves the per-page working-set / plan-budget / in-flight-budget /
//! spill knobs off config, builds the per-call memory + disk governors,
//! invokes [`crate::render::rebuild_pages`], and records the duration +
//! governor observations.
//!
//! the disk governor is constructed and observed but not yet plumbed into
//! `render::rebuild_pages` (spill admission is currently unbounded under
//! disk pressure). surfaced as `_disk_governor` rather than dropped on the
//! floor so a future patch wiring it through `render::rebuild_pages`
//! lands at one named site instead of having to rediscover the
//! construction call. TODO(compile-disk-governor): pass &disk_governor
//! into render::rebuild_pages once the executor accepts it.

use std::collections::HashMap;

use mars_observability::Metrics;
use mars_types::{BindingId, Manifest};

use crate::incremental::DirtyPages;
use crate::plan::BootstrapPlan;
use crate::render::{self, RebuildOutcome};
use crate::sidecar::SidecarReader;
use crate::stages::shared::governors;
use crate::{CompilerError, Deps};

pub(crate) async fn run(
    deps: &Deps,
    cfg: &mars_config::Compiler,
    metrics: &Metrics,
    plan: &BootstrapPlan,
    prior: &Manifest,
    sidecars: &HashMap<BindingId, SidecarReader<'_>>,
    dirty: DirtyPages,
) -> Result<RebuildOutcome, CompilerError> {
    let working_set_bytes = cfg.compile_page_working_set()?;
    let plan_budget_bytes = cfg.compile_plan_budget()?;
    let in_flight_budget_bytes = cfg.compile_in_flight_pages_budget()?;
    let spill_dir = cfg.compile_spill_dir_path();
    let spill_open_file_limit = cfg.compile_spill_open_file_limit;
    let governor = governors::build_memory_governor(cfg)?;
    let _disk_governor = governors::build_disk_governor(cfg)?;

    let started = std::time::Instant::now();
    let outcome = render::rebuild_pages(
        deps,
        plan,
        prior,
        sidecars,
        dirty,
        working_set_bytes,
        plan_budget_bytes,
        in_flight_budget_bytes,
        &spill_dir,
        spill_open_file_limit,
        &governor,
    )
    .await?;
    metrics.observe_compiler_rebuild_duration(started.elapsed());
    governors::log_memory_observations("compile.cycle.governor", &governor);
    governors::log_disk_observations("compile.cycle.disk_governor", &_disk_governor);
    Ok(outcome)
}

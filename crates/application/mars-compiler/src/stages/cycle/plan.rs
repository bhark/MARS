//! cycle stage 1: build [`CycleCtx`] - resolve the prior manifest, the
//! bootstrap plan, every binding's page-membership sidecar, the compile
//! knobs, and the per-call memory + disk governors.

use crate::stages::ctx::{CompileKnobs, CycleCtx};
use crate::stages::shared::governors;
use crate::stages::shared::sidecars::OwnedSidecars;
use crate::{Compiler, CompilerError};

pub(crate) async fn build(c: &Compiler) -> Result<CycleCtx, CompilerError> {
    let prior = c.deps.manifest.current().await?.ok_or(CompilerError::NoPriorManifest {
        context: "run_cycle_once",
    })?;
    let plan = crate::plan::build_bootstrap_plan(&c.config)?;
    let sidecars = OwnedSidecars::fetch(c.deps.store.as_ref(), &prior.bindings).await?;
    let knobs = CompileKnobs {
        working_set_bytes: c.config.compiler.compile_page_working_set()?,
        plan_budget_bytes: c.config.compiler.compile_plan_budget()?,
        in_flight_budget_bytes: c.config.compiler.compile_in_flight_pages_budget()?,
        binding_parallelism: c.config.compiler.compile_binding_parallelism,
        spill_dir: c.config.compiler.compile_spill_dir_path(),
        spill_open_file_limit: c.config.compiler.compile_spill_open_file_limit,
    };
    Ok(CycleCtx {
        plan,
        prior,
        sidecars,
        knobs,
        mem_governor: governors::build_memory_governor(&c.config.compiler)?,
        disk_governor: governors::build_disk_governor(&c.config.compiler)?,
        failure_policy: c.config.compiler.binding_failure_policy,
    })
}

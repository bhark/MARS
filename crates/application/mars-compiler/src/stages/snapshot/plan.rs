//! snapshot stage 1: build the [`SnapshotCtx`].
//!
//! resolves the bootstrap plan, the next manifest version, the compile
//! knobs, and constructs the memory + disk governors.

use crate::stages::ctx::{CompileKnobs, SnapshotCtx};
use crate::stages::shared::{governors, parallelism};
use crate::{Compiler, CompilerError};

pub(crate) async fn build(c: &Compiler) -> Result<SnapshotCtx, CompilerError> {
    let plan = crate::plan::build_bootstrap_plan(&c.config)?;
    let prev_version = c.deps.manifest.current().await?.map_or(0, |m| m.version);
    let next_version = prev_version + 1;
    let knobs = CompileKnobs {
        working_set_bytes: c.config.compiler.compile_page_working_set()?,
        plan_budget_bytes: c.config.compiler.compile_plan_budget()?,
        in_flight_budget_bytes: c.config.compiler.compile_in_flight_pages_budget()?,
        binding_parallelism: parallelism::resolve_binding_parallelism(&c.config.compiler, &c.config.sources),
        spill_dir: c.config.compiler.compile_spill_dir_path(),
    };
    Ok(SnapshotCtx {
        plan,
        service_name: c.config.service.name.clone(),
        next_version,
        knobs,
        mem_governor: governors::build_memory_governor(&c.config.compiler)?,
        disk_governor: governors::build_disk_governor(&c.config.compiler)?,
    })
}

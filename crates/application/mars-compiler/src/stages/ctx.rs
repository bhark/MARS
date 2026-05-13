//! per-pipeline context bundles.
//!
//! three distinct types (one per pipeline) instead of one shared blob so
//! that the type system rejects cross-pipeline misuse: a snapshot has no
//! prior manifest, a rebalance never builds an `IncrementalCycle` and
//! never reads `cycle_counter`. fields specific to one pipeline live only
//! on that ctx; shared knobs live in [`CompileKnobs`].
//!
//! `cycle_counter` deliberately does NOT live on any ctx. it stays on
//! `Compiler` because its reset-on-leader-handover semantics depend on
//! the lock guard's lifetime, not the per-call lifetime; the cycle's
//! reconcile-cadence stage takes `&Compiler` instead of `&CycleCtx` for
//! that reason.

use std::path::PathBuf;

use crate::disk_governor::DiskGovernor;
use crate::memory_governor::MemoryGovernor;
use crate::plan::BootstrapPlan;

/// shared compile knobs read off config. snapshot uses every field;
/// cycle uses every field except `binding_parallelism` (cycle does not
/// run the unified compile pipeline directly; only the truncate fallback
/// inside the rebuild stage does).
pub(crate) struct CompileKnobs {
    pub(crate) working_set_bytes: u64,
    pub(crate) plan_budget_bytes: u64,
    pub(crate) in_flight_budget_bytes: u64,
    pub(crate) binding_parallelism: usize,
    pub(crate) spill_dir: PathBuf,
    pub(crate) spill_open_file_limit: usize,
}

pub(crate) struct SnapshotCtx {
    pub(crate) plan: BootstrapPlan,
    pub(crate) service_name: String,
    pub(crate) next_version: u64,
    pub(crate) knobs: CompileKnobs,
    pub(crate) mem_governor: MemoryGovernor,
    pub(crate) disk_governor: DiskGovernor,
}

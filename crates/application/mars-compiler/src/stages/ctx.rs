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

use mars_types::Manifest;

use crate::disk_governor::DiskGovernor;
use crate::memory_governor::MemoryGovernor;
use crate::plan::BootstrapPlan;
use crate::stages::shared::sidecars::OwnedSidecars;

/// typical number of pass-2 pages a single binding keeps partially
/// filled - hence with an open spill file - at once. multiplied by
/// `binding_parallelism` to size the global spill open-file budget.
const PER_BINDING_ACTIVE_PAGES: usize = 128;

/// shared compile knobs read off config. snapshot and cycle both honour
/// every field: `binding_parallelism` bounds whole-binding compile
/// concurrency in the snapshot pipeline and per-binding rebuild
/// concurrency inside the cycle's rebuild stage (truncate and
/// incremental alike), and also derives the spill open-file ceiling.
pub(crate) struct CompileKnobs {
    pub(crate) working_set_bytes: u64,
    pub(crate) plan_budget_bytes: u64,
    pub(crate) in_flight_budget_bytes: u64,
    pub(crate) binding_parallelism: usize,
    pub(crate) spill_dir: PathBuf,
}

impl CompileKnobs {
    /// spill open-file ceiling, derived from `binding_parallelism`.
    pub(crate) const fn spill_open_file_limit(&self) -> usize {
        self.binding_parallelism.saturating_mul(PER_BINDING_ACTIVE_PAGES)
    }
}

pub(crate) struct SnapshotCtx {
    pub(crate) plan: BootstrapPlan,
    pub(crate) service_name: String,
    pub(crate) next_version: u64,
    pub(crate) knobs: CompileKnobs,
    pub(crate) mem_governor: MemoryGovernor,
    pub(crate) disk_governor: DiskGovernor,
}

pub(crate) struct CycleCtx {
    pub(crate) plan: BootstrapPlan,
    pub(crate) prior: Manifest,
    pub(crate) sidecars: OwnedSidecars,
    pub(crate) knobs: CompileKnobs,
    pub(crate) mem_governor: MemoryGovernor,
    pub(crate) disk_governor: DiskGovernor,
    pub(crate) failure_policy: mars_config::BindingFailurePolicy,
}

pub(crate) struct RebalanceCtx {
    pub(crate) plan: BootstrapPlan,
    pub(crate) prior: Manifest,
    pub(crate) working_set_bytes: u64,
}

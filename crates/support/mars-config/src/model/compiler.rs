use std::num::NonZeroUsize;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ConfigError;
use crate::units;

/// Compiler settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Compiler {
    /// Window over which incremental change events are batched before
    /// publishing a manifest. Unit-suffixed duration (`5min`, `30s`).
    #[serde(default = "default_compiler_window")]
    pub window: String,
    /// **Deprecated.** Cell-substrate concurrency knob. Ignored under the
    /// page-keyed substrate; accepted for backward compatibility.
    #[serde(default)]
    pub parallel_cells: Option<NonZeroUsize>,
    /// Per-page hydrated-row working-set ceiling enforced during pass-2
    /// page assembly (rebuild and bootstrap-from-plan). Crossing this
    /// ceiling trips [`CompilerError::ScratchBudgetExceeded`].
    /// Unit-suffixed byte literal (`256MiB`).
    ///
    /// [`CompilerError::ScratchBudgetExceeded`]: https://docs.rs/mars-compiler
    #[serde(default = "default_compile_page_working_set")]
    pub compile_page_working_set_bytes: String,
    /// Hard ceiling on pass-1 page-planner allocation
    /// (`row_count × size_of::<PlanRow>()`). Crossing this ceiling trips
    /// [`CompilerError::BootstrapPlanTooLarge`] before the planner
    /// allocates beyond it. Unit-suffixed byte literal (`8GiB`).
    ///
    /// [`CompilerError::BootstrapPlanTooLarge`]: https://docs.rs/mars-compiler
    #[serde(default = "default_compile_plan_budget")]
    pub compile_plan_budget_bytes: String,
    /// Number of bindings the snapshot pipeline compiles concurrently.
    /// Each in-flight binding holds one `CompileSession` (one pooled
    /// connection in `REPEATABLE READ`), so `source.pool.max_size` must
    /// allow at least this many concurrent checkouts plus headroom for
    /// sidecar / object-store I/O.
    #[serde(default = "default_compile_binding_parallelism")]
    pub compile_binding_parallelism: usize,
    /// Hard ceiling on pass-2 RAM allocation, summed across the whole
    /// compile pipeline (all in-flight bindings). When unset, the compiler
    /// self-sizes against the active cgroup memory limit: 70% of the limit
    /// minus a 512 MiB OS / runtime reservation. Outside a cgroup the
    /// fallback is 2 GiB. Unit-suffixed byte literal (`4GiB`).
    ///
    /// Treat as "throughput knob": setting this lower makes pass 2 spill
    /// more aggressively to disk; it never crashes the process.
    #[serde(default)]
    pub compile_memory_budget_bytes: Option<String>,
    /// Hard ceiling on transient compile-scratch disk usage summed across
    /// all in-flight bindings. When unset the governor admits up to a
    /// generous fallback (64 GiB) so existing deployments behave as
    /// before; setting this lower makes the disk-spill paths backpressure
    /// rather than ENOSPC. Unit-suffixed byte literal (`32GiB`).
    #[serde(default)]
    pub compile_disk_budget_bytes: Option<String>,
    /// Soft trigger threshold for pass-2 disk spill, per binding. Pass 2
    /// streams the whole table once per binding and buckets rows into the
    /// planned pages; pages eager-flush on completion. When the summed
    /// footprint of partially-filled in-memory pages crosses this
    /// threshold, the compiler spills all current partial buffers to
    /// per-page files under [`compile_spill_dir`] and continues. Pages
    /// that complete before the trigger fires never touch disk.
    /// Unit-suffixed byte literal (`256MiB`).
    ///
    /// [`compile_spill_dir`]: Self::compile_spill_dir
    #[serde(default = "default_compile_in_flight_pages_budget")]
    pub compile_in_flight_pages_budget_bytes: String,
    /// Directory used as scratch for pass-2 disk spill files. Each binding
    /// gets a uniquely-named subdirectory underneath, removed at session
    /// end (success or failure). When unset, resolves to
    /// `${TMPDIR}/mars-compile-spill`. The directory must be writable and
    /// have headroom for the worst-case spilled hydrated payload across
    /// concurrent bindings - typical sizing is a few GiB per spilling
    /// binding, multiplied by [`compile_binding_parallelism`].
    ///
    /// [`compile_binding_parallelism`]: Self::compile_binding_parallelism
    #[serde(default)]
    pub compile_spill_dir: Option<String>,
    /// Maximum number of spill files held open at once. The compiler keeps
    /// recently-written spill files open for buffered append; older entries
    /// are flushed and closed when the limit is reached. Sized for typical
    /// `compile_binding_parallelism` × per-binding active page set; raise
    /// if profiling shows reopen syscall churn dominating the spill path.
    #[serde(default = "default_compile_spill_open_file_limit")]
    pub compile_spill_open_file_limit: usize,
    /// Opportunistic rebalance settings (split / merge under size or
    /// bbox-dilation drift).
    #[serde(default)]
    pub rebalance: Rebalance,
}

impl Default for Compiler {
    fn default() -> Self {
        Self {
            window: default_compiler_window(),
            parallel_cells: None,
            compile_page_working_set_bytes: default_compile_page_working_set(),
            compile_plan_budget_bytes: default_compile_plan_budget(),
            compile_binding_parallelism: default_compile_binding_parallelism(),
            compile_memory_budget_bytes: None,
            compile_disk_budget_bytes: None,
            compile_in_flight_pages_budget_bytes: default_compile_in_flight_pages_budget(),
            compile_spill_dir: None,
            compile_spill_open_file_limit: default_compile_spill_open_file_limit(),
            rebalance: Rebalance::default(),
        }
    }
}

impl Compiler {
    /// Resolve `window` to a `Duration`.
    pub fn window_dur(&self) -> Result<Duration, ConfigError> {
        units::parse_duration(&self.window)
    }

    /// Resolve `compile_page_working_set_bytes` to bytes.
    pub fn compile_page_working_set(&self) -> Result<u64, ConfigError> {
        units::parse_bytes(&self.compile_page_working_set_bytes)
    }

    /// Resolve `compile_plan_budget_bytes` to bytes.
    pub fn compile_plan_budget(&self) -> Result<u64, ConfigError> {
        units::parse_bytes(&self.compile_plan_budget_bytes)
    }

    /// Resolve `compile_memory_budget_bytes` to bytes when explicitly set.
    pub fn compile_memory_budget(&self) -> Result<Option<u64>, ConfigError> {
        self.compile_memory_budget_bytes
            .as_deref()
            .map(units::parse_bytes)
            .transpose()
    }

    /// Resolve `compile_disk_budget_bytes` to bytes when explicitly set.
    pub fn compile_disk_budget(&self) -> Result<Option<u64>, ConfigError> {
        self.compile_disk_budget_bytes
            .as_deref()
            .map(units::parse_bytes)
            .transpose()
    }

    /// Resolve `compile_in_flight_pages_budget_bytes` to bytes.
    pub fn compile_in_flight_pages_budget(&self) -> Result<u64, ConfigError> {
        units::parse_bytes(&self.compile_in_flight_pages_budget_bytes)
    }

    /// Resolve `compile_spill_dir` against the platform default
    /// (`${TMPDIR}/mars-compile-spill`). Pure path computation; does not
    /// create the directory.
    #[must_use]
    pub fn compile_spill_dir_path(&self) -> std::path::PathBuf {
        match &self.compile_spill_dir {
            Some(s) => std::path::PathBuf::from(s),
            None => std::env::temp_dir().join("mars-compile-spill"),
        }
    }
}

fn default_compiler_window() -> String {
    "5min".to_owned()
}

fn default_compile_page_working_set() -> String {
    "256MiB".to_owned()
}

fn default_compile_plan_budget() -> String {
    "8GiB".to_owned()
}

fn default_compile_binding_parallelism() -> usize {
    2
}

fn default_compile_in_flight_pages_budget() -> String {
    "256MiB".to_owned()
}

fn default_compile_spill_open_file_limit() -> usize {
    256
}

/// Opportunistic rebalance settings. Rebalance is
/// decoupled from the hot edit path; it runs at most once per binding per
/// maintenance window or on operator command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rebalance {
    /// Whether the periodic rebalance window is active. Off by default
    /// (opportunistic-only; operator command path remains usable).
    #[serde(default)]
    pub enabled: bool,
    /// Cadence of the rebalance window. Unit-suffixed duration (`1d`, `12h`).
    #[serde(default = "default_rebalance_window")]
    pub window: String,
}

impl Default for Rebalance {
    fn default() -> Self {
        Self {
            enabled: false,
            window: default_rebalance_window(),
        }
    }
}

impl Rebalance {
    /// Resolve `window` to a `Duration`.
    pub fn window_dur(&self) -> Result<Duration, ConfigError> {
        units::parse_duration(&self.window)
    }
}

fn default_rebalance_window() -> String {
    "1d".to_owned()
}

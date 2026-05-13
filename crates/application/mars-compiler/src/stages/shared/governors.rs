//! pipeline-scoped admission caps and their observation logs.
//!
//! both the memory and disk governors are built per compile run (snapshot,
//! cycle, rebalance), threaded through stages by reference, and logged on
//! drop. cap resolution is centralised here so the snapshot and cycle
//! paths cannot disagree on fallback order.

use crate::CompilerError;
use crate::disk_governor::DiskGovernor;
use crate::memory_governor::MemoryGovernor;

/// memory-governor cap resolution. order: explicit
/// `compile_memory_budget_bytes` > 70% of detected cgroup limit minus
/// 512 MiB OS reservation > 2 GiB fallback.
pub(crate) fn resolve_memory_cap(cfg: &mars_config::Compiler) -> Result<u64, CompilerError> {
    const RESERVATION_BYTES: u64 = 512 * 1024 * 1024;
    const FALLBACK_BYTES: u64 = 2 * 1024 * 1024 * 1024;
    const MIN_CAP_BYTES: u64 = 64 * 1024 * 1024;

    if let Some(explicit) = cfg.compile_memory_budget()? {
        tracing::info!(
            target: "mars_compiler::compile",
            cap_resolved = explicit,
            source = "config",
            "compile.memory_governor: cap resolved"
        );
        return Ok(explicit);
    }
    if let Some(limit) = mars_config::cgroup::detect_memory_limit() {
        let after_reservation = limit.saturating_sub(RESERVATION_BYTES);
        let scaled = (after_reservation as u128 * 7 / 10) as u64;
        let cap = scaled.max(MIN_CAP_BYTES);
        tracing::info!(
            target: "mars_compiler::compile",
            cap_resolved = cap,
            cgroup_limit = limit,
            source = "cgroup",
            "compile.memory_governor: cap resolved"
        );
        return Ok(cap);
    }
    tracing::info!(
        target: "mars_compiler::compile",
        cap_resolved = FALLBACK_BYTES,
        source = "default",
        "compile.memory_governor: cap resolved"
    );
    Ok(FALLBACK_BYTES)
}

/// disk-governor cap resolution. order: explicit
/// `compile_disk_budget_bytes` > 64 GiB fallback. (filesystem free-space
/// introspection is operator-controlled; piping `nix::statvfs` here would
/// add an unsafe-code carve-out the compiler crate avoids.)
pub(crate) fn resolve_disk_cap(cfg: &mars_config::Compiler) -> Result<u64, CompilerError> {
    const FALLBACK_BYTES: u64 = 64 * 1024 * 1024 * 1024;
    if let Some(explicit) = cfg.compile_disk_budget()? {
        tracing::info!(
            target: "mars_compiler::compile",
            cap_resolved = explicit,
            source = "config",
            "compile.disk_governor: cap resolved"
        );
        return Ok(explicit);
    }
    tracing::info!(
        target: "mars_compiler::compile",
        cap_resolved = FALLBACK_BYTES,
        source = "default",
        "compile.disk_governor: cap resolved"
    );
    Ok(FALLBACK_BYTES)
}

/// build a [`MemoryGovernor`] sized via [`resolve_memory_cap`].
pub(crate) fn build_memory_governor(cfg: &mars_config::Compiler) -> Result<MemoryGovernor, CompilerError> {
    Ok(MemoryGovernor::new(resolve_memory_cap(cfg)?))
}

/// build a [`DiskGovernor`] sized via [`resolve_disk_cap`].
pub(crate) fn build_disk_governor(cfg: &mars_config::Compiler) -> Result<DiskGovernor, CompilerError> {
    Ok(DiskGovernor::new(resolve_disk_cap(cfg)?))
}

pub(crate) fn log_memory_observations(event: &'static str, governor: &MemoryGovernor) {
    tracing::info!(
        target: "mars_compiler::compile",
        cap_bytes = governor.cap_bytes(),
        peak_bytes = governor.peak_bytes(),
        acquire_wait_us = governor.acquire_wait_us(),
        "{event}",
    );
}

pub(crate) fn log_disk_observations(event: &'static str, governor: &DiskGovernor) {
    tracing::info!(
        target: "mars_compiler::compile",
        cap_bytes = governor.cap_bytes(),
        peak_bytes = governor.peak_bytes(),
        acquire_wait_us = governor.acquire_wait_us(),
        "{event}",
    );
}

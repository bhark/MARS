//! Runtime pixel-budget resolution.
//!
//! mirrors the compiler's `resolve_memory_cap`: when `render.pixel_budget` is
//! unset the runtime self-sizes the in-flight pixmap permit pool against the
//! pod's cgroup memory limit. resolution order is explicit config >
//! cgroup-derived > fallback.

use mars_config::{ConfigError, Render, cgroup};

// runtime overhead carved off before the pixmap share (decoded geometry,
// cache index, fonts, tokio).
const RESERVATION_BYTES: u64 = 256 * 1024 * 1024;
// fallback when no cgroup limit is detectable; preserves the historical
// 512MiB default.
const FALLBACK_PERMITS: u32 = (512 * 1024 * 1024) / 4;
// floor so a tight cgroup still leaves a usable budget.
const MIN_PERMITS: u32 = (64 * 1024 * 1024) / 4;

/// Resolve the runtime pixel budget to a permit count. Order: explicit
/// `render.pixel_budget` > 40% of the cgroup limit minus a 256 MiB
/// reservation > 512 MiB fallback.
pub fn resolve_pixel_budget(render: &Render) -> Result<u32, ConfigError> {
    if let Some(explicit) = render.pixel_budget_permits()? {
        tracing::info!(
            target: "mars_runtime::budget",
            budget_resolved = explicit,
            source = "config",
            "render.pixel_budget: resolved"
        );
        return Ok(explicit);
    }
    if let Some(limit) = cgroup::detect_memory_limit() {
        let permits = budget_from_limit(limit);
        tracing::info!(
            target: "mars_runtime::budget",
            budget_resolved = permits,
            cgroup_limit = limit,
            source = "cgroup",
            "render.pixel_budget: resolved"
        );
        return Ok(permits);
    }
    tracing::info!(
        target: "mars_runtime::budget",
        budget_resolved = FALLBACK_PERMITS,
        source = "default",
        "render.pixel_budget: resolved"
    );
    Ok(FALLBACK_PERMITS)
}

// 40% of the cgroup limit minus the reservation, as a pixel-permit count.
fn budget_from_limit(limit: u64) -> u32 {
    let after_reservation = limit.saturating_sub(RESERVATION_BYTES);
    let pixmap_bytes = (u128::from(after_reservation) * 2 / 5) as u64;
    let permits = u32::try_from(pixmap_bytes / 4).unwrap_or(u32::MAX);
    permits.max(MIN_PERMITS)
}

#[cfg(test)]
mod tests;

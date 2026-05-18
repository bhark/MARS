//! Pod / cgroup memory limit detection.
//!
//! The compiler self-sizes its memory governor cap against the cgroup limit
//! the pod runs under. Detection is best-effort: outside a cgroup, on parse
//! errors, or when the limit is the sentinel "max" / `u64::MAX`-class value
//! (meaning unconstrained), the helper returns `None` and callers fall back
//! to a conservative default.
//!
//! cgroup v2 surfaces the limit at `/sys/fs/cgroup/memory.max`; v1 at
//! `/sys/fs/cgroup/memory/memory.limit_in_bytes`. v2 wins when both exist.

use std::path::{Path, PathBuf};

const CGROUP_V2_PATH: &str = "/sys/fs/cgroup/memory.max";
const CGROUP_V1_PATH: &str = "/sys/fs/cgroup/memory/memory.limit_in_bytes";

// kernel sentinel for "no limit" on cgroup v1; effectively u64::MAX rounded
// to the page size. anything within ~1 EiB is treated as "unconstrained".
const UNCONSTRAINED_THRESHOLD: u64 = 1 << 60;

/// Detect the active cgroup memory limit in bytes, if any.
#[must_use]
pub fn detect_memory_limit() -> Option<u64> {
    detect_from_paths(&[PathBuf::from(CGROUP_V2_PATH), PathBuf::from(CGROUP_V1_PATH)])
}

fn detect_from_paths(paths: &[PathBuf]) -> Option<u64> {
    paths.iter().find_map(|p| read_limit(p))
}

fn read_limit(path: &Path) -> Option<u64> {
    let raw = std::fs::read_to_string(path).ok()?;
    parse_limit(raw.trim())
}

fn parse_limit(s: &str) -> Option<u64> {
    if s.eq_ignore_ascii_case("max") {
        return None;
    }
    let v: u64 = s.parse().ok()?;
    if v >= UNCONSTRAINED_THRESHOLD {
        return None;
    }
    Some(v)
}

#[cfg(test)]
mod tests;

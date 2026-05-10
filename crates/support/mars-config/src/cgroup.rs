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
#[allow(clippy::unwrap_used)]
mod tests {
    use std::io::Write;

    use super::*;

    fn write_temp(name: &str, contents: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("mars-cgroup-test-{name}-{}", std::process::id()));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        p
    }

    #[test]
    fn parse_numeric() {
        assert_eq!(parse_limit("1073741824"), Some(1_073_741_824));
    }

    #[test]
    fn parse_max_sentinel() {
        assert_eq!(parse_limit("max"), None);
        assert_eq!(parse_limit("MAX"), None);
    }

    #[test]
    fn parse_unconstrained_v1() {
        // typical v1 "unlimited" value: u64::MAX rounded down to page boundary.
        assert_eq!(parse_limit("9223372036854771712"), None);
        assert_eq!(parse_limit(&u64::MAX.to_string()), None);
    }

    #[test]
    fn parse_garbage() {
        assert_eq!(parse_limit("not a number"), None);
        assert_eq!(parse_limit(""), None);
    }

    #[test]
    fn detect_reads_first_existing() {
        let v2 = write_temp("v2", "536870912\n");
        let v1 = write_temp("v1", "1073741824\n");
        assert_eq!(detect_from_paths(&[v2.clone(), v1.clone()]), Some(536_870_912));
        std::fs::remove_file(&v2).ok();
        assert_eq!(detect_from_paths(&[v2.clone(), v1.clone()]), Some(1_073_741_824));
        std::fs::remove_file(&v1).ok();
        assert_eq!(detect_from_paths(&[v2, v1]), None);
    }

    #[test]
    fn detect_skips_max_sentinel() {
        let v2 = write_temp("v2-max", "max\n");
        let v1 = write_temp("v1-num", "2147483648\n");
        assert_eq!(detect_from_paths(&[v2.clone(), v1.clone()]), Some(2_147_483_648));
        std::fs::remove_file(&v2).ok();
        std::fs::remove_file(&v1).ok();
    }
}

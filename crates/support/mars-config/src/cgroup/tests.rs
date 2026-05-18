#![allow(clippy::unwrap_used)]

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

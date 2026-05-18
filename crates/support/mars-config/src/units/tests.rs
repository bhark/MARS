#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn bytes_kib_mib_gib() {
    assert_eq!(parse_bytes("1KiB").unwrap(), 1024);
    assert_eq!(parse_bytes("12.5KiB").unwrap(), 12_800);
    assert_eq!(parse_bytes("1MiB").unwrap(), 1024 * 1024);
    assert_eq!(parse_bytes("50GiB").unwrap(), 50u64 * 1024 * 1024 * 1024);
    assert_eq!(parse_bytes("100").unwrap(), 100);
}

#[test]
fn distance_m_km() {
    assert!((parse_distance_m("4096m").unwrap() - 4096.0).abs() < f64::EPSILON);
    assert!((parse_distance_m("2.5km").unwrap() - 2500.0).abs() < f64::EPSILON);
}

#[test]
fn duration_min_s() {
    assert_eq!(parse_duration("5min").unwrap(), Duration::from_secs(300));
    assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
}

#[test]
fn rejects_bad_unit() {
    assert!(parse_bytes("12foo").is_err());
    assert!(parse_distance_m("12foo").is_err());
    assert!(parse_duration("nope").is_err());
}

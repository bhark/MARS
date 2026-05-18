#![allow(clippy::unwrap_used)]
use super::*;

#[tokio::test]
async fn admits_within_cap_and_tracks_peak() {
    let g = DiskGovernor::new(1024);
    let r1 = g.acquire(512).await.unwrap();
    let r2 = g.acquire(256).await.unwrap();
    assert_eq!(g.in_flight_bytes(), 768);
    assert_eq!(g.peak_bytes(), 768);
    drop(r1);
    assert_eq!(g.in_flight_bytes(), 256);
    drop(r2);
    assert_eq!(g.in_flight_bytes(), 0);
}

#[tokio::test]
async fn try_acquire_returns_none_under_pressure() {
    let g = DiskGovernor::new(1024);
    let _full = g.acquire(1024).await.unwrap();
    assert!(g.try_acquire(1).is_none());
}

#![allow(clippy::unwrap_used, clippy::panic)]

use std::time::Duration;

use super::*;

#[tokio::test]
async fn admits_within_cap_and_tracks_peak() {
    let g = MemoryGovernor::new(1024);
    let r1 = g.acquire(512).await.unwrap();
    let r2 = g.acquire(256).await.unwrap();
    assert_eq!(g.in_flight_bytes(), 768);
    assert_eq!(g.peak_bytes(), 768);
    drop(r1);
    assert_eq!(g.in_flight_bytes(), 256);
    // peak is monotonic.
    assert_eq!(g.peak_bytes(), 768);
    drop(r2);
    assert_eq!(g.in_flight_bytes(), 0);
}

#[tokio::test]
async fn try_acquire_returns_none_under_pressure() {
    let g = MemoryGovernor::new(1024);
    let _full = g.acquire(1024).await.unwrap();
    assert!(g.try_acquire(1).is_none());
}

#[tokio::test]
async fn release_via_drop_unblocks_waiters() {
    let g = MemoryGovernor::new(1024);
    let held = g.acquire(1024).await.unwrap();
    let g2 = g.clone();
    let waiter = tokio::spawn(async move { g2.acquire(512).await });
    // give the waiter a chance to park.
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!waiter.is_finished());
    drop(held);
    let reservation = waiter.await.unwrap().unwrap();
    assert_eq!(reservation.bytes(), 512);
    assert!(g.acquire_wait_us() > 0);
}

#[tokio::test]
async fn clones_share_state() {
    let g = MemoryGovernor::new(2048);
    let g2 = g.clone();
    let r = g.acquire(1024).await.unwrap();
    assert_eq!(g2.in_flight_bytes(), 1024);
    drop(r);
    assert_eq!(g2.in_flight_bytes(), 0);
}

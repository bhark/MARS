//! Shared byte-permit semaphore for compiler scratch-disk pressure.
//!
//! Mirrors [`crate::memory_governor::MemoryGovernor`] but tracks bytes
//! written to `compile_spill_dir` instead of bytes resident in RAM. All
//! disk-write sites (the row spill, the route index, the sidecar arena,
//! the external-sort runs) acquire bytes before writing and release them
//! when the data they buffer no longer needs the disk space.
//!
//! A misconfigured disk budget makes the compiler slow (acquires block
//! waiting for prior reservations to drain), never crashes - same
//! contract as the memory governor.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore};

/// Shared byte-permit semaphore. Cheap to clone.
#[derive(Debug, Clone)]
pub struct DiskGovernor {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    sem: Arc<Semaphore>,
    cap_bytes: u64,
    in_flight: AtomicU64,
    peak: AtomicU64,
    wait_us_total: AtomicU64,
}

impl DiskGovernor {
    /// Build a governor admitting up to `cap_bytes` simultaneous bytes.
    #[must_use]
    pub fn new(cap_bytes: u64) -> Self {
        let requested = usize::try_from(cap_bytes).unwrap_or(usize::MAX);
        let cap_permits = requested.min(Semaphore::MAX_PERMITS);
        Self {
            inner: Arc::new(Inner {
                sem: Arc::new(Semaphore::new(cap_permits)),
                cap_bytes,
                in_flight: AtomicU64::new(0),
                peak: AtomicU64::new(0),
                wait_us_total: AtomicU64::new(0),
            }),
        }
    }

    #[must_use]
    pub fn cap_bytes(&self) -> u64 {
        self.inner.cap_bytes
    }

    #[must_use]
    pub fn peak_bytes(&self) -> u64 {
        self.inner.peak.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn in_flight_bytes(&self) -> u64 {
        self.inner.in_flight.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn acquire_wait_us(&self) -> u64 {
        self.inner.wait_us_total.load(Ordering::Relaxed)
    }

    pub async fn acquire(&self, bytes: u64) -> Result<DiskReservation, AcquireError> {
        let permits = clamp_permits(bytes);
        let started = Instant::now();
        let permit = Arc::clone(&self.inner.sem).acquire_many_owned(permits).await?;
        let waited_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.inner.wait_us_total.fetch_add(waited_us, Ordering::Relaxed);
        let now = self
            .inner
            .in_flight
            .fetch_add(bytes, Ordering::Relaxed)
            .saturating_add(bytes);
        bump_max(&self.inner.peak, now);
        Ok(DiskReservation {
            bytes,
            inner: Arc::clone(&self.inner),
            _permit: permit,
        })
    }

    #[must_use]
    pub fn try_acquire(&self, bytes: u64) -> Option<DiskReservation> {
        let permits = clamp_permits(bytes);
        let permit = Arc::clone(&self.inner.sem).try_acquire_many_owned(permits).ok()?;
        let now = self
            .inner
            .in_flight
            .fetch_add(bytes, Ordering::Relaxed)
            .saturating_add(bytes);
        bump_max(&self.inner.peak, now);
        Some(DiskReservation {
            bytes,
            inner: Arc::clone(&self.inner),
            _permit: permit,
        })
    }
}

/// Drop-released byte reservation against a [`DiskGovernor`].
#[derive(Debug)]
pub struct DiskReservation {
    bytes: u64,
    inner: Arc<Inner>,
    _permit: OwnedSemaphorePermit,
}

impl DiskReservation {
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for DiskReservation {
    fn drop(&mut self) {
        self.inner.in_flight.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

fn clamp_permits(bytes: u64) -> u32 {
    u32::try_from(bytes).unwrap_or(u32::MAX)
}

fn bump_max(target: &AtomicU64, observed: u64) {
    let mut current = target.load(Ordering::Relaxed);
    while observed > current {
        match target.compare_exchange_weak(current, observed, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }
}

#[cfg(test)]
mod tests {
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
}

//! shared byte-permit semaphore for compiler memory pressure.
//!
//! a misconfigured byte budget should make the compiler slow, not crash. the
//! governor admits byte-sized requests against a configured ceiling and
//! backpressures awaiting callers when the ceiling is saturated. modelled on
//! the runtime's `render_sem` pattern (a `tokio::sync::Semaphore` used as a
//! counted resource); each permit corresponds to one byte.
//!
//! piece 1 wires the governor as a behavioural no-op: the cap defaults to the
//! existing `compile_in_flight_pages_budget_bytes`, so backpressure trips at
//! the same threshold the spill path already enforces. later pieces narrow
//! the consumer set so a saturated governor produces backpressure rather
//! than the current hard-fail budget errors.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore};

/// Shared byte-permit semaphore. Cheap to clone; clones share the same cap
/// and counters.
#[derive(Debug, Clone)]
pub struct MemoryGovernor {
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

impl MemoryGovernor {
    /// build a governor admitting up to `cap_bytes` simultaneous bytes.
    /// the underlying `Semaphore` is sized in bytes; tokio caps a semaphore
    /// at `Semaphore::MAX_PERMITS`, so very large requested caps are clamped
    /// down (effectively "no admission ceiling" for our purposes).
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

    /// configured admission ceiling.
    #[must_use]
    pub fn cap_bytes(&self) -> u64 {
        self.inner.cap_bytes
    }

    /// observed peak in-flight reservation since construction.
    #[must_use]
    pub fn peak_bytes(&self) -> u64 {
        self.inner.peak.load(Ordering::Relaxed)
    }

    /// bytes currently held by outstanding reservations.
    #[must_use]
    pub fn in_flight_bytes(&self) -> u64 {
        self.inner.in_flight.load(Ordering::Relaxed)
    }

    /// cumulative microseconds callers have spent blocked on `acquire`.
    #[must_use]
    pub fn acquire_wait_us(&self) -> u64 {
        self.inner.wait_us_total.load(Ordering::Relaxed)
    }

    /// reserve `bytes`, awaiting if the cap is saturated. on success the
    /// returned reservation releases its bytes back when dropped.
    pub async fn acquire(&self, bytes: u64) -> Result<MemoryReservation, AcquireError> {
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
        Ok(MemoryReservation {
            bytes,
            inner: Arc::clone(&self.inner),
            _permit: permit,
        })
    }

    /// non-blocking variant. returns `None` when admission would block.
    #[must_use]
    pub fn try_acquire(&self, bytes: u64) -> Option<MemoryReservation> {
        let permits = clamp_permits(bytes);
        let permit = Arc::clone(&self.inner.sem).try_acquire_many_owned(permits).ok()?;
        let now = self
            .inner
            .in_flight
            .fetch_add(bytes, Ordering::Relaxed)
            .saturating_add(bytes);
        bump_max(&self.inner.peak, now);
        Some(MemoryReservation {
            bytes,
            inner: Arc::clone(&self.inner),
            _permit: permit,
        })
    }
}

/// Drop-released byte reservation against a [`MemoryGovernor`].
#[derive(Debug)]
pub struct MemoryReservation {
    bytes: u64,
    inner: Arc<Inner>,
    _permit: OwnedSemaphorePermit,
}

impl MemoryReservation {
    /// reserved byte count.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        self.inner.in_flight.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

// tokio::Semaphore caps a single acquire at u32::MAX permits; saturate
// rather than refuse. with cap_bytes ≤ u32::MAX (the current operating
// envelope) this branch is unreachable; guards future >4 GiB caps.
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
mod tests;

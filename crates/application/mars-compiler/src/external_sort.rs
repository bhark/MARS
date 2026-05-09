//! Working-set guard for the rebuild path's per-page hydration pass.
//!
//! The unified compile pipeline hydrates one page's worth of feature ids at
//! a time and asserts the accumulated row bytes stay under the configured
//! ceiling. The guard is intentionally tiny: bookkeeping plus a saturating
//! threshold check.

/// Tracks accumulated row-byte estimates against a configured ceiling.
/// Designed for the per-page hydration loop: callers update the guard once
/// per row and check it before keeping the row in memory.
#[derive(Debug)]
pub struct WorkingSetGuard {
    ceiling_bytes: u64,
    observed_bytes: u64,
}

impl WorkingSetGuard {
    /// Build a guard for the given ceiling.
    #[must_use]
    pub fn new(ceiling_bytes: u64) -> Self {
        Self {
            ceiling_bytes,
            observed_bytes: 0,
        }
    }

    /// Add `delta` to the observed total. Returns `Err(observed)` when the
    /// new total would exceed the ceiling so the caller can route the
    /// failure into a named error.
    pub fn add(&mut self, delta: u64) -> Result<(), u64> {
        let new_total = self.observed_bytes.saturating_add(delta);
        if new_total > self.ceiling_bytes {
            self.observed_bytes = new_total;
            return Err(new_total);
        }
        self.observed_bytes = new_total;
        Ok(())
    }

    /// Current accumulated total.
    #[must_use]
    pub fn observed(&self) -> u64 {
        self.observed_bytes
    }

    /// Configured ceiling.
    #[must_use]
    pub fn ceiling(&self) -> u64 {
        self.ceiling_bytes
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn working_set_guard_admits_under_ceiling() {
        let mut g = WorkingSetGuard::new(1_000);
        assert!(g.add(400).is_ok());
        assert!(g.add(500).is_ok());
        assert_eq!(g.observed(), 900);
    }

    #[test]
    fn working_set_guard_rejects_over_ceiling() {
        let mut g = WorkingSetGuard::new(1_000);
        assert!(g.add(800).is_ok());
        let res = g.add(300);
        assert!(res.is_err());
        // observed updated even on rejection so the named error carries the
        // correct overrun number.
        assert_eq!(g.observed(), 1100);
    }

    #[test]
    fn working_set_guard_saturates_on_overflow() {
        let mut g = WorkingSetGuard::new(u64::MAX);
        assert!(g.add(u64::MAX).is_ok());
        assert!(g.add(u64::MAX).is_ok());
        assert_eq!(g.observed(), u64::MAX);
    }
}

//! Pass-2 per-page in-memory state, keyed by `(level_index, PageId)`.
//!
//! Collapses six parallel `HashMap`s that the row-streaming loop in
//! [`super::pass2`] used to thread side-by-side: the expected row count
//! (read-only after setup), in-memory kept and pruned row buffers, the
//! received counter, the per-page byte estimate, and the governor
//! reservations shadowing those bytes. Centralising them here keeps the
//! row loop reading at the level of operations - push, record arrival,
//! take, evict, verify - instead of multiplexing across map literals.
//!
//! The `RouteIndex` (build-phase target map, frozen to disk for the row
//! stream) and the `SpillManager` (per-page on-disk overflow files) stay
//! separate: they have distinct lifetimes and concerns.

use std::collections::HashMap;

use mars_types::PageId;

use crate::CompilerError;
use crate::disk_governor::DiskGovernor;
use crate::memory_governor::{MemoryGovernor, MemoryReservation};
use crate::spill::SpillManager;

use super::KeyedRow;

pub(super) struct PageAccumulator {
    expected: HashMap<(usize, PageId), usize>,
    partial: HashMap<(usize, PageId), Vec<KeyedRow>>,
    pruned: HashMap<(usize, PageId), Vec<KeyedRow>>,
    received: HashMap<(usize, PageId), usize>,
    page_bytes: HashMap<(usize, PageId), u64>,
    page_reservations: HashMap<(usize, PageId), Vec<MemoryReservation>>,
    in_flight_bytes: u64,
}

impl PageAccumulator {
    pub(super) fn new() -> Self {
        Self {
            expected: HashMap::new(),
            partial: HashMap::new(),
            pruned: HashMap::new(),
            received: HashMap::new(),
            page_bytes: HashMap::new(),
            page_reservations: HashMap::new(),
            in_flight_bytes: 0,
        }
    }

    pub(super) fn set_expected(&mut self, lvl_idx: usize, page_id: PageId, count: usize) {
        self.expected.insert((lvl_idx, page_id), count);
    }

    /// Push optionally-kept and optionally-pruned rows into the in-memory
    /// buffers, charge the page's byte estimate against the running
    /// `in_flight_bytes` total, and try to back the charge with a governor
    /// reservation. A failed `try_acquire` is silently ignored - the
    /// in-flight budget still trips spill independently.
    pub(super) fn push(
        &mut self,
        lvl_idx: usize,
        page_id: PageId,
        kept: Option<KeyedRow>,
        pruned: Option<KeyedRow>,
        kr_bytes: u64,
        governor: &MemoryGovernor,
    ) {
        if let Some(kr) = kept {
            self.partial.entry((lvl_idx, page_id)).or_default().push(kr);
        }
        if let Some(kr) = pruned {
            self.pruned.entry((lvl_idx, page_id)).or_default().push(kr);
        }
        *self.page_bytes.entry((lvl_idx, page_id)).or_insert(0) += kr_bytes;
        self.in_flight_bytes = self.in_flight_bytes.saturating_add(kr_bytes);
        if let Some(res) = governor.try_acquire(kr_bytes) {
            self.page_reservations.entry((lvl_idx, page_id)).or_default().push(res);
        }
    }

    /// Bump the received counter for `(lvl_idx, page_id)` and return true
    /// when the count just hit its expected total. Always call once per
    /// route hit, regardless of whether the row landed in memory or in
    /// spill - the counter tracks page completion, not buffer location.
    pub(super) fn record_arrival(&mut self, lvl_idx: usize, page_id: PageId) -> bool {
        let r = self.received.entry((lvl_idx, page_id)).or_insert(0);
        *r += 1;
        // missing expected (unknown route) is impossible in practice: the
        // route index is built from the same plan as `expected`. fall back
        // to false rather than panicking; verify_complete catches any drift.
        self.expected.get(&(lvl_idx, page_id)).is_some_and(|exp| *r == *exp)
    }

    /// Remove and return the in-memory buffers for a completed page,
    /// dropping its governor reservation and decrementing
    /// `in_flight_bytes` by the page's recorded estimate.
    pub(super) fn take(&mut self, lvl_idx: usize, page_id: PageId) -> (Vec<KeyedRow>, Vec<KeyedRow>) {
        let kept = self.partial.remove(&(lvl_idx, page_id)).unwrap_or_default();
        let pruned = self.pruned.remove(&(lvl_idx, page_id)).unwrap_or_default();
        let bytes = self.page_bytes.remove(&(lvl_idx, page_id)).unwrap_or(0);
        self.in_flight_bytes = self.in_flight_bytes.saturating_sub(bytes);
        self.page_reservations.remove(&(lvl_idx, page_id));
        (kept, pruned)
    }

    pub(super) fn in_flight_bytes(&self) -> u64 {
        self.in_flight_bytes
    }

    /// Evict every in-memory partial / pruned buffer to per-page spill
    /// files and drop all governor reservations - the soft trigger when
    /// `in_flight_bytes` crosses its budget. Spill writes are admitted
    /// against `disk_governor`; the reservations carry on the spill
    /// manager until per-page drain.
    pub(super) async fn evict_to_spill(
        &mut self,
        spill: &mut SpillManager,
        disk_governor: &DiskGovernor,
    ) -> Result<(), CompilerError> {
        let evicted = spill
            .flush_all_partials(&mut self.partial, &mut self.pruned, &mut self.page_bytes, disk_governor)
            .await?;
        self.in_flight_bytes = self.in_flight_bytes.saturating_sub(evicted);
        self.page_reservations.clear();
        Ok(())
    }

    /// End-of-stream guard: every expected page must have received its
    /// full row count. A short stream would otherwise leave silent gaps
    /// in the binding's substrate.
    pub(super) fn verify_complete(&self) -> Result<(), CompilerError> {
        for (route, exp) in &self.expected {
            let got = self.received.get(route).copied().unwrap_or(0);
            if got != *exp {
                return Err(CompilerError::InvariantViolation {
                    what: "rebuild_from_plan: full-table stream returned fewer rows than the snapshot plan",
                });
            }
        }
        Ok(())
    }
}

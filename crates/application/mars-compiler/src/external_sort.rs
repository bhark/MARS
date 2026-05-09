//! Bucketed Hilbert-key sort for bootstrap row sets.
//!
//! Two responsibilities:
//!
//! 1. enforce a configurable scratch budget so a binding whose row
//!    accumulation outgrows operator-allocated scratch hits a clear, named
//!    [`crate::CompilerError::ScratchBudgetExceeded`] instead of OOM-ing the
//!    pod (LAZARUS bailout 5);
//! 2. sort rows by `HilbertKey` using a top-bit bucket pass followed by
//!    in-bucket comparison sort. The bucket structure is the same the
//!    on-disk spill backend will use; once we have measured forvaltning2
//!    on production data and a binding warrants the disk path, only the
//!    bucket *materialisation* changes (memory `Vec` → memory-mapped temp
//!    file). The ordering algorithm and tests carry over unchanged.
//!
//! Disk-backed external sort itself is a tracked carry-over: forvaltning2's
//! largest binding fits the 4 GiB default working-set ceiling, and lifting
//! it requires serialising `KeyedRow` (or re-streaming the source twice).
//! Both options have non-trivial tradeoffs; we land them when measurement
//! demands it, not before, and the operator-facing failure mode is
//! explicit.

use mars_types::HilbertKey;

/// Configuration for [`bucketed_sort_in_place`] and [`WorkingSetGuard`].
#[derive(Debug, Clone, Copy)]
pub struct ExternalSortConfig {
    /// Working-set ceiling in bytes. The bootstrap accumulator drains its
    /// in-memory tail to per-bucket spill files when crossed; the rebuild
    /// path (which does not spill) fails with
    /// [`crate::CompilerError::ScratchBudgetExceeded`] on overflow.
    pub working_set_bytes: u64,
    /// Number of leading bits of the Hilbert key used for the bucket pass.
    /// Higher → more, smaller buckets; default 12 → 4096 buckets is well
    /// matched to the L2 cache size on modern x86 cores while keeping
    /// bucket count below the open-file-descriptor budget once the disk
    /// backend lands.
    pub bucket_bits: u8,
}

impl ExternalSortConfig {
    /// Default for production use: 4 GiB ceiling, 12-bit bucket pass.
    pub const DEFAULT: Self = Self {
        working_set_bytes: 4 * 1024 * 1024 * 1024,
        bucket_bits: 12,
    };
}

/// Tracks accumulated row-byte estimates against a configured ceiling.
/// Designed for the streaming bootstrap path: callers update the guard
/// once per row and check it before keeping the row in memory.
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

/// Sort `rows` ascending by the key returned by `key_of`, in-place,
/// using a top-`bucket_bits` bucket pass followed by an in-bucket
/// comparison sort. Stable within a bucket only — equal keys may be
/// reordered relative to other equal keys, matching the existing
/// `sort_by_key` semantics callers rely on (the page-build pipeline
/// re-orders by `feature_id` later).
///
/// `key_of` is a closure returning `HilbertKey` for a row. Passed by
/// closure (not via a trait) so callers do not need to commit to a
/// particular `KeyedRow` shape — the same routine sorts the snapshot
/// rows today and will sort spill-bucket entries when the disk backend
/// lands.
pub fn bucketed_sort_in_place<T, F>(rows: &mut Vec<T>, bucket_bits: u8, key_of: F)
where
    F: Fn(&T) -> HilbertKey,
{
    if rows.len() < 2 {
        return;
    }
    let bits = bucket_bits.clamp(0, 16);
    if bits == 0 {
        rows.sort_by_key(&key_of);
        return;
    }
    let bucket_count = 1usize << bits;
    let shift = 64 - u32::from(bits);

    // count pass.
    let mut counts = vec![0usize; bucket_count];
    let bucket_idx = |row: &T| -> usize {
        let key_bits: u64 = key_of(row).get();
        (key_bits >> shift) as usize
    };
    for r in rows.iter() {
        counts[bucket_idx(r)] += 1;
    }

    // exclusive-prefix-sum into start offsets, retain a copy as final
    // bucket boundaries for the per-bucket sort below.
    let mut starts = Vec::with_capacity(bucket_count + 1);
    let mut acc = 0usize;
    for c in &counts {
        starts.push(acc);
        acc += *c;
    }
    starts.push(acc);

    // counting-sort scatter. clone the row out once via swap_remove rotation
    // to avoid moving from a shared mutable borrow. simplest correct path is
    // to allocate a parallel destination vec.
    let mut sorted: Vec<Option<T>> = (0..rows.len()).map(|_| None).collect();
    let mut cursor = starts.clone();
    for r in rows.drain(..) {
        let bucket_bits_value: u64 = key_of(&r).get();
        let b = (bucket_bits_value >> shift) as usize;
        let slot = cursor[b];
        cursor[b] += 1;
        sorted[slot] = Some(r);
    }
    rows.extend(sorted.into_iter().flatten());

    // in-bucket comparison sort. each bucket spans [starts[b]..starts[b+1]].
    for b in 0..bucket_count {
        let lo = starts[b];
        let hi = starts[b + 1];
        if hi - lo > 1 {
            rows[lo..hi].sort_by_key(&key_of);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Row {
        key: HilbertKey,
        tag: u32,
    }

    fn k(v: u64) -> HilbertKey {
        HilbertKey::new(v)
    }

    #[test]
    fn bucketed_sort_orders_keys_ascending() {
        let mut rows = vec![
            Row {
                key: k(0xF0F0_F0F0_0000_0000),
                tag: 1,
            },
            Row {
                key: k(0x0102_0304_0506_0708),
                tag: 2,
            },
            Row {
                key: k(0xAAAA_AAAA_AAAA_AAAA),
                tag: 3,
            },
            Row {
                key: k(0x0000_0000_0000_0001),
                tag: 4,
            },
        ];
        bucketed_sort_in_place(&mut rows, 12, |r| r.key);
        let _ = rows[0].tag;
        let keys: Vec<u64> = rows.iter().map(|r| r.key.get()).collect();
        assert_eq!(
            keys,
            vec![
                0x0000_0000_0000_0001,
                0x0102_0304_0506_0708,
                0xAAAA_AAAA_AAAA_AAAA,
                0xF0F0_F0F0_0000_0000,
            ]
        );
    }

    #[test]
    fn bucketed_sort_matches_naive_for_random_inputs() {
        let mut rows: Vec<Row> = (0..2048u64)
            .map(|i| Row {
                key: k(i.wrapping_mul(2_654_435_761).rotate_left(13)),
                tag: i as u32,
            })
            .collect();
        let mut expected = rows.clone();
        expected.sort_by_key(|a| a.key);
        bucketed_sort_in_place(&mut rows, 12, |r| r.key);
        assert_eq!(rows, expected);
    }

    #[test]
    fn bucketed_sort_handles_duplicate_keys() {
        let mut rows: Vec<Row> = (0..16u32)
            .map(|i| Row {
                key: k(0x4242_0000_0000_0000),
                tag: i,
            })
            .collect();
        bucketed_sort_in_place(&mut rows, 12, |r| r.key);
        // all keys equal; tags preserved in some order, total count unchanged.
        assert_eq!(rows.len(), 16);
        let mut tags: Vec<u32> = rows.iter().map(|r| r.tag).collect();
        tags.sort_unstable();
        assert_eq!(tags, (0..16u32).collect::<Vec<_>>());
    }

    #[test]
    fn bucketed_sort_handles_short_input() {
        let mut empty: Vec<Row> = Vec::new();
        bucketed_sort_in_place(&mut empty, 12, |r| r.key);
        assert!(empty.is_empty());

        let mut single = vec![Row { key: k(7), tag: 0 }];
        bucketed_sort_in_place(&mut single, 12, |r| r.key);
        assert_eq!(single.len(), 1);
    }

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
        // observed is updated even on rejection so the named error carries
        // the correct overrun number.
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

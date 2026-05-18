#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

fn key(seed: u8) -> SourceRowKey {
    SourceRowKey::from_bytes([seed; 16])
}

fn key_le(idx: u8) -> SourceRowKey {
    let mut b = [0u8; 16];
    b[15] = idx;
    SourceRowKey::from_bytes(b)
}

#[test]
fn frozen_round_trip_in_memory_only() {
    let g = MemoryGovernor::new(64 * 1024 * 1024);
    let mut idx = RouteIndex::with_governor(&g, std::env::temp_dir().as_path()).unwrap();
    idx.insert(key(1), (0, PageId(10))).unwrap();
    idx.insert(key(1), (1, PageId(20))).unwrap();
    idx.insert(key(2), (0, PageId(30))).unwrap();
    let (mut cur, stats) = idx.freeze().unwrap();
    assert_eq!(stats.entries_total, 2);
    assert_eq!(stats.runs_merged, 1);
    let r = cur.advance_to(&key(1)).unwrap().unwrap();
    assert_eq!(r, vec![(0, PageId(10)), (1, PageId(20))]);
    let r = cur.advance_to(&key(2)).unwrap().unwrap();
    assert_eq!(r, vec![(0, PageId(30))]);
    assert!(cur.advance_to(&key(3)).unwrap().is_none());
}

#[test]
fn frozen_round_trip_with_spilled_runs() {
    // tight cap forces every batch beyond the floor to spill. use the
    // floor itself as the cap so the very first overrun spills.
    let g = MemoryGovernor::new(MIN_SPILL_BATCH_BYTES);
    let mut idx = RouteIndex::with_governor(&g, std::env::temp_dir().as_path()).unwrap();
    // insert a substantial number of keys in ascending order so multiple
    // spills happen; a single key bump is small (~92 B) so we need many.
    for i in 0..255u8 {
        idx.insert(key_le(i), (i as usize, PageId(u64::from(i)))).unwrap();
    }
    let (mut cur, _stats) = idx.freeze().unwrap();
    for i in 0..255u8 {
        let r = cur.advance_to(&key_le(i)).unwrap().unwrap();
        assert_eq!(r, vec![(i as usize, PageId(u64::from(i)))]);
    }
}

#[test]
fn cursor_returns_none_for_unindexed_keys() {
    let g = MemoryGovernor::new(64 * 1024 * 1024);
    let mut idx = RouteIndex::with_governor(&g, std::env::temp_dir().as_path()).unwrap();
    idx.insert(key_le(2), (0, PageId(20))).unwrap();
    idx.insert(key_le(4), (0, PageId(40))).unwrap();
    let (mut cur, _) = idx.freeze().unwrap();
    assert!(cur.advance_to(&key_le(1)).unwrap().is_none());
    let r = cur.advance_to(&key_le(2)).unwrap().unwrap();
    assert_eq!(r, vec![(0, PageId(20))]);
    assert!(cur.advance_to(&key_le(3)).unwrap().is_none());
    let r = cur.advance_to(&key_le(4)).unwrap().unwrap();
    assert_eq!(r, vec![(0, PageId(40))]);
    assert!(cur.advance_to(&key_le(5)).unwrap().is_none());
}

#[test]
fn cursor_rejects_non_monotonic_advance() {
    let g = MemoryGovernor::new(64 * 1024 * 1024);
    let mut idx = RouteIndex::with_governor(&g, std::env::temp_dir().as_path()).unwrap();
    idx.insert(key_le(1), (0, PageId(10))).unwrap();
    idx.insert(key_le(2), (0, PageId(20))).unwrap();
    let (mut cur, _) = idx.freeze().unwrap();
    cur.advance_to(&key_le(2)).unwrap();
    let err = cur.advance_to(&key_le(1)).unwrap_err();
    assert!(matches!(err, CompilerError::InvariantViolation { .. }));
}

#[test]
fn freeze_concatenates_routes_across_inmem_and_spilled_run() {
    let g = MemoryGovernor::new(MIN_SPILL_BATCH_BYTES);
    let mut idx = RouteIndex::with_governor(&g, std::env::temp_dir().as_path()).unwrap();
    idx.insert(key_le(7), (0, PageId(1))).unwrap();
    // saturate cap to force a spill that includes key_le(7).
    for i in 8..255u8 {
        idx.insert(key_le(i), (0, PageId(u64::from(i)))).unwrap();
    }
    // reinsert key_le(7) into the post-spill in-memory map so freeze has
    // to merge two sources for the same key.
    idx.insert(key_le(7), (1, PageId(99))).unwrap();
    let (mut cur, _) = idx.freeze().unwrap();
    let r = cur.advance_to(&key_le(7)).unwrap().unwrap();
    assert!(r.contains(&(0, PageId(1))));
    assert!(r.contains(&(1, PageId(99))));
    assert_eq!(r.len(), 2);
}

#[test]
fn min_spill_batch_floor_prevents_tiny_runs() {
    // governor cap of 1 byte: every try_grow fails. before the floor we
    // accept inserts unaccounted; after the floor the spill_to_run path
    // would trigger -- but at this scale (a handful of entries) we never
    // hit the floor, so no spill is produced, the in-mem map just grows.
    let g = MemoryGovernor::new(1);
    let mut idx = RouteIndex::with_governor(&g, std::env::temp_dir().as_path()).unwrap();
    for i in 0..32u8 {
        idx.insert(key_le(i), (0, PageId(u64::from(i)))).unwrap();
    }
    assert!(idx.runs.is_empty(), "no spill should fire below the floor");
    let (mut cur, stats) = idx.freeze().unwrap();
    // freeze flushes the residual in-memory map as a single run.
    assert_eq!(stats.runs_merged, 1);
    for i in 0..32u8 {
        let r = cur.advance_to(&key_le(i)).unwrap().unwrap();
        assert_eq!(r, vec![(0, PageId(u64::from(i)))]);
    }
}

#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::Arc;

use mars_artifact::{FeatureGeom, GeomKind};
use mars_source::AttrValue;
use mars_types::{HilbertKey, PageId};
use tempfile::TempDir;

use super::*;

fn sample_row(seed: u64) -> KeyedRow {
    KeyedRow {
        feature: FeatureGeom {
            user_id: seed,
            bbox: [seed as f32, seed as f32 + 1.0, seed as f32 + 2.0, seed as f32 + 3.0],
            geom: GeomKind::Polygon(vec![vec![(1.0, 2.0), (3.0, 4.0), (5.0, 6.0), (1.0, 2.0)]]),
        },
        attrs: Arc::new(vec![
            ("name".into(), AttrValue::String(format!("row-{seed}"))),
            ("count".into(), AttrValue::Int(seed as i64 * 2)),
            ("ratio".into(), AttrValue::Float(seed as f64 / 3.0)),
            ("flag".into(), AttrValue::Bool(seed.is_multiple_of(2))),
            ("missing".into(), AttrValue::Null),
        ]),
        geom_bytes_estimate: 256 + seed,
        key: HilbertKey::new(seed.wrapping_mul(7919)),
        row_fingerprint: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15),
    }
}

fn rows_eq(a: &KeyedRow, b: &KeyedRow) -> bool {
    a.feature == b.feature
        && a.geom_bytes_estimate == b.geom_bytes_estimate
        && a.key == b.key
        && a.row_fingerprint == b.row_fingerprint
        && a.attrs.as_slice() == b.attrs.as_slice()
}

fn unbounded_governor() -> DiskGovernor {
    DiskGovernor::new(u64::MAX)
}

#[tokio::test]
async fn roundtrip_kept_and_pruned() {
    let parent = TempDir::new().unwrap();
    let mut spill = SpillManager::new(parent.path(), 32).unwrap();
    let dg = unbounded_governor();
    let r1 = sample_row(1);
    let r2 = sample_row(2);
    let r3 = sample_row(3);
    spill
        .append(0, PageId::new(7), SpillKind::Kept, &r1, &dg)
        .await
        .unwrap();
    spill
        .append(0, PageId::new(7), SpillKind::Pruned, &r2, &dg)
        .await
        .unwrap();
    spill
        .append(0, PageId::new(7), SpillKind::Kept, &r3, &dg)
        .await
        .unwrap();
    assert!(spill.is_spilled(0, PageId::new(7)));
    let (kept, pruned) = spill.drain(0, PageId::new(7)).unwrap();
    assert_eq!(kept.len(), 2);
    assert_eq!(pruned.len(), 1);
    assert!(rows_eq(&kept[0], &r1));
    assert!(rows_eq(&kept[1], &r3));
    assert!(rows_eq(&pruned[0], &r2));
    assert!(!spill.is_spilled(0, PageId::new(7)));
    // every reservation released back to the governor after drain.
    assert_eq!(dg.in_flight_bytes(), 0);
}

#[tokio::test]
async fn lru_eviction_reopens_in_append_mode() {
    let parent = TempDir::new().unwrap();
    let mut spill = SpillManager::new(parent.path(), 2).unwrap();
    let dg = unbounded_governor();
    // populate three pages; LRU = 2 forces an eviction.
    spill
        .append(0, PageId::new(1), SpillKind::Kept, &sample_row(10), &dg)
        .await
        .unwrap();
    spill
        .append(0, PageId::new(2), SpillKind::Kept, &sample_row(20), &dg)
        .await
        .unwrap();
    spill
        .append(0, PageId::new(3), SpillKind::Kept, &sample_row(30), &dg)
        .await
        .unwrap();
    // touch page 1 again; it was evicted, must reopen and append without
    // overwriting the header.
    spill
        .append(0, PageId::new(1), SpillKind::Kept, &sample_row(11), &dg)
        .await
        .unwrap();
    let (kept, _) = spill.drain(0, PageId::new(1)).unwrap();
    assert_eq!(kept.len(), 2);
    assert!(rows_eq(&kept[0], &sample_row(10)));
    assert!(rows_eq(&kept[1], &sample_row(11)));
}

#[tokio::test]
async fn drop_removes_dir() {
    let parent = TempDir::new().unwrap();
    let dg = unbounded_governor();
    let dir_path = {
        let mut spill = SpillManager::new(parent.path(), 4).unwrap();
        spill
            .append(0, PageId::new(0), SpillKind::Kept, &sample_row(1), &dg)
            .await
            .unwrap();
        spill.dir.path().to_path_buf()
    };
    assert!(!dir_path.exists(), "binding tempdir should be removed on drop");
    // dropping the spill manager releases every still-held reservation.
    assert_eq!(dg.in_flight_bytes(), 0);
}

#[tokio::test]
async fn flush_all_partials_evicts_and_clears_maps() {
    let parent = TempDir::new().unwrap();
    let mut spill = SpillManager::new(parent.path(), 16).unwrap();
    let dg = unbounded_governor();
    let mut partial: HashMap<(usize, PageId), Vec<KeyedRow>> = HashMap::new();
    let mut pruned: HashMap<(usize, PageId), Vec<KeyedRow>> = HashMap::new();
    let mut page_bytes: HashMap<(usize, PageId), u64> = HashMap::new();
    partial.insert((0, PageId::new(0)), vec![sample_row(1), sample_row(2)]);
    partial.insert((0, PageId::new(1)), vec![sample_row(3)]);
    pruned.insert((0, PageId::new(0)), vec![sample_row(99)]);
    page_bytes.insert((0, PageId::new(0)), 1000);
    page_bytes.insert((0, PageId::new(1)), 500);
    let evicted = spill
        .flush_all_partials(&mut partial, &mut pruned, &mut page_bytes, &dg)
        .await
        .unwrap();
    assert_eq!(evicted, 1500);
    assert!(partial.is_empty());
    assert!(pruned.is_empty());
    assert!(page_bytes.is_empty());
    assert!(spill.metrics().triggered);
    let (kept0, pruned0) = spill.drain(0, PageId::new(0)).unwrap();
    assert_eq!(kept0.len(), 2);
    assert_eq!(pruned0.len(), 1);
    let (kept1, pruned1) = spill.drain(0, PageId::new(1)).unwrap();
    assert_eq!(kept1.len(), 1);
    assert!(pruned1.is_empty());
    assert_eq!(dg.in_flight_bytes(), 0);
}

#[tokio::test]
async fn admission_under_tight_budget_records_peak_and_wait() {
    // a tight cap forces the second append to block until the first
    // page drains; peak must reach the budget, and acquire_wait_us
    // must be non-zero.
    let parent = TempDir::new().unwrap();
    let mut spill = SpillManager::new(parent.path(), 8).unwrap();
    // budget is small enough that two simultaneous pages can't fit but
    // one row + its header fits comfortably.
    let dg = DiskGovernor::new(512);
    spill
        .append(0, PageId::new(1), SpillKind::Kept, &sample_row(1), &dg)
        .await
        .unwrap();
    assert!(dg.in_flight_bytes() > 0);
    let peak_after_first = dg.peak_bytes();
    assert!(peak_after_first > 0);

    // hold a reservation that exhausts the rest of the budget, then
    // spawn a waiter that tries to append more bytes. the waiter must
    // not complete until the held reservation drops.
    let hold = dg.acquire(dg.cap_bytes() - dg.in_flight_bytes()).await.unwrap();

    let dg_clone = dg.clone();
    let parent_path = parent.path().to_path_buf();
    let waiter = tokio::spawn(async move {
        let mut spill2 = SpillManager::new(&parent_path, 4).unwrap();
        spill2
            .append(0, PageId::new(2), SpillKind::Kept, &sample_row(2), &dg_clone)
            .await
            .unwrap();
        spill2.drain(0, PageId::new(2)).unwrap();
    });

    // small grace so the waiter parks on the semaphore.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert!(!waiter.is_finished(), "waiter must block while budget is exhausted");
    drop(hold);
    waiter.await.unwrap();
    assert!(dg.acquire_wait_us() > 0, "tight budget should accumulate wait time");
}

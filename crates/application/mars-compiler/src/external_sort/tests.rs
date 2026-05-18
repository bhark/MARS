#![allow(clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use mars_artifact::{FeatureGeom, GeomKind};
use mars_source::AttrValue;

use super::*;

fn sample(seed: u64) -> KeyedRow {
    KeyedRow {
        feature: FeatureGeom {
            user_id: seed,
            bbox: [seed as f32, seed as f32, seed as f32, seed as f32],
            geom: GeomKind::Polygon(vec![vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]]),
        },
        attrs: Arc::new(vec![
            ("name".into(), AttrValue::String(format!("row-{seed}"))),
            ("count".into(), AttrValue::Int(seed as i64)),
        ]),
        geom_bytes_estimate: 128 + seed,
        key: HilbertKey::new(seed.wrapping_mul(0xDEAD_BEEF)),
        row_fingerprint: seed.wrapping_mul(31),
    }
}

fn rows_eq(a: &KeyedRow, b: &KeyedRow) -> bool {
    a.feature == b.feature
        && a.geom_bytes_estimate == b.geom_bytes_estimate
        && a.key == b.key
        && a.row_fingerprint == b.row_fingerprint
        && a.attrs.as_slice() == b.attrs.as_slice()
}

#[test]
fn fast_path_sorts_in_memory() {
    let rows: Vec<KeyedRow> = (0..32u64).rev().map(sample).collect();
    let g = MemoryGovernor::new(64 * 1024 * 1024);
    let sorted = external_sort_page(rows.clone(), 4096, std::env::temp_dir().as_path(), &g).unwrap();
    let mut expected = rows;
    expected.sort_by(keyed_row_cmp);
    assert_eq!(sorted.len(), expected.len());
    for (a, b) in sorted.iter().zip(&expected) {
        assert!(rows_eq(a, b));
    }
}

#[test]
fn slow_path_round_trip_matches_in_memory_sort() {
    let rows: Vec<KeyedRow> = (0..256u64).map(|i| sample(i.wrapping_mul(11) % 199)).collect();
    // tight cap + small chunk forces the chunked spill path.
    let g = MemoryGovernor::new(64);
    let sorted = external_sort_page(rows.clone(), 1024, std::env::temp_dir().as_path(), &g).unwrap();
    let mut expected = rows;
    expected.sort_by(keyed_row_cmp);
    assert_eq!(sorted.len(), expected.len());
    for (a, b) in sorted.iter().zip(&expected) {
        assert!(rows_eq(a, b), "spill path diverged from in-memory sort");
    }
}

#[test]
fn empty_input_round_trips_through_both_paths() {
    let g = MemoryGovernor::new(64 * 1024 * 1024);
    let r = external_sort_page(Vec::new(), 1024, std::env::temp_dir().as_path(), &g).unwrap();
    assert!(r.is_empty());
    let g = MemoryGovernor::new(0);
    let r = external_sort_page(Vec::new(), 1024, std::env::temp_dir().as_path(), &g).unwrap();
    assert!(r.is_empty());
}

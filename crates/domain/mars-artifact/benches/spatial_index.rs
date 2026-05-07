//! microbenches for the packed hilbert r-tree.
//!
//! gates phase a of the lazarus substrate pivot. the r-tree query alone
//! must beat a linear walk over the same inputs by ≥ 50× at typical
//! viewport selectivity on 40k-feature pages.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::hint::black_box;

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mars_artifact::{DEFAULT_NODE_SIZE, SpatialIndex, SpatialIndexBuilder};

const SMALL: usize = 40_000;
const LARGE: usize = 200_000;
const SPACING: f32 = 10.0;

fn make_grid(n: usize) -> Vec<(u32, [f32; 4])> {
    let side = (n as f64).sqrt().ceil() as usize;
    let mut items = Vec::with_capacity(n);
    for k in 0..n {
        let i = (k % side) as f32;
        let j = (k / side) as f32;
        items.push((
            k as u32,
            [i * SPACING, j * SPACING, i * SPACING + 1.0, j * SPACING + 1.0],
        ));
    }
    items
}

fn build_index(items: &[(u32, [f32; 4])]) -> Bytes {
    let mut b = SpatialIndexBuilder::new(DEFAULT_NODE_SIZE).unwrap();
    for &(idx, bb) in items {
        b.add(idx, bb);
    }
    b.finish().unwrap()
}

fn page_extent(n: usize) -> f32 {
    (n as f64).sqrt().ceil() as f32 * SPACING
}

/// query bbox sized so the area ≈ target_hits grid cells (each cell area ≈ 100).
fn query_for_hits(n: usize, target_hits: usize) -> [f32; 4] {
    let area = (target_hits as f32) * (SPACING * SPACING);
    let half = (area.sqrt() / 2.0).max(1.0);
    let center = page_extent(n) * 0.5;
    [center - half, center - half, center + half, center + half]
}

fn bench_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("spatial_index_build");
    for &n in &[SMALL, LARGE] {
        let items = make_grid(n);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &items, |b, items| {
            b.iter(|| {
                let bytes = build_index(black_box(items));
                black_box(bytes);
            });
        });
    }
    group.finish();
}

fn bench_query(c: &mut Criterion) {
    let mut group = c.benchmark_group("spatial_index_query");
    for &n in &[SMALL, LARGE] {
        let items = make_grid(n);
        let idx = SpatialIndex::open(build_index(&items)).unwrap();
        for &target in &[10usize, 100, 1000, 10_000] {
            let q = query_for_hits(n, target);
            let mut hits = Vec::new();
            idx.query(q, &mut hits);
            let actual = hits.len();
            group.throughput(Throughput::Elements(actual as u64));
            let label = format!("n_{n}_sel_{target}_actual_{actual}");
            group.bench_function(label, |b| {
                let mut out = Vec::with_capacity(actual + 16);
                b.iter(|| {
                    out.clear();
                    idx.query(black_box(q), &mut out);
                    black_box(&out);
                });
            });
        }
    }
    group.finish();
}

fn bench_brute_force_baseline(c: &mut Criterion) {
    let mut group = c.benchmark_group("spatial_index_brute_force_baseline");
    let n = SMALL;
    let items = make_grid(n);
    let bboxes: Vec<[f32; 4]> = items.iter().map(|&(_, bb)| bb).collect();
    for &target in &[10usize, 100, 1000, 10_000] {
        let q = query_for_hits(n, target);
        let actual = bboxes
            .iter()
            .filter(|bb| bb[0] <= q[2] && bb[1] <= q[3] && bb[2] >= q[0] && bb[3] >= q[1])
            .count();
        group.throughput(Throughput::Elements(actual as u64));
        let label = format!("n_{n}_sel_{target}_actual_{actual}");
        group.bench_function(label, |b| {
            let mut out = Vec::with_capacity(actual + 16);
            b.iter(|| {
                out.clear();
                let q = black_box(q);
                for (i, bb) in bboxes.iter().enumerate() {
                    if bb[0] <= q[2] && bb[1] <= q[3] && bb[2] >= q[0] && bb[3] >= q[1] {
                        out.push(i as u32);
                    }
                }
                black_box(&out);
            });
        });
    }
    group.finish();
}

fn bench_query_dilated_bbox(c: &mut Criterion) {
    // pathology: one elongated feature inflating its leaf's parent bbox 10x.
    // worst case for prune efficiency; correctness is unaffected.
    let mut group = c.benchmark_group("spatial_index_query_dilated");
    let n = SMALL;
    let mut items = make_grid(n);
    let extent = page_extent(n);
    items.push((
        items.len() as u32,
        [0.0, extent * 0.5, extent, extent * 0.5 + 1.0],
    ));
    let idx = SpatialIndex::open(build_index(&items)).unwrap();
    let q = query_for_hits(n, 500);
    let mut hits = Vec::new();
    idx.query(q, &mut hits);
    let actual = hits.len();
    group.throughput(Throughput::Elements(actual as u64));
    let label = format!("n_{n}_plus_1elongated_sel_500_actual_{actual}");
    group.bench_function(label, |b| {
        let mut out = Vec::with_capacity(actual + 16);
        b.iter(|| {
            out.clear();
            idx.query(black_box(q), &mut out);
            black_box(&out);
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_build,
    bench_query,
    bench_brute_force_baseline,
    bench_query_dilated_bbox,
);
criterion_main!(benches);

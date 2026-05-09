//! microbenches for the packed hilbert r-tree.
//!
//! gates phase a of the lazarus substrate pivot. the r-tree query alone
//! must beat a linear walk over the same inputs by ≥ 50× at typical
//! viewport selectivity on 40k-feature pages.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::hint::black_box;

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mars_artifact::{
    DEFAULT_NODE_SIZE, FeatureGeom, FeatureIndexEntry, GeomKind, SpatialIndex, SpatialIndexBuilder, decode_one_geom,
    encode_geometry_payload, iter_feature_index,
};

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

/// linear bbox-filter walk over the GeometryPayload feature index.
/// SpatialIndex's R-tree is the substitute; this is the comparator the
/// LAZARUS Phase A gate measures against.
fn bench_geometry_payload_linear_walk(c: &mut Criterion) {
    let mut group = c.benchmark_group("spatial_index_geometry_payload_linear_walk");
    let n = SMALL;
    let ring_len = 16;
    let features: Vec<FeatureGeom> = (0..n)
        .map(|k| {
            let side = (n as f64).sqrt().ceil();
            let i = (k as f64) % side;
            let j = (k as f64) / side;
            let cx = i * f64::from(SPACING);
            let cy = j * f64::from(SPACING);
            let mut ring = Vec::with_capacity(ring_len + 1);
            for v in 0..ring_len {
                let theta = (v as f64) * std::f64::consts::TAU / (ring_len as f64);
                ring.push((cx + theta.cos() * 0.5, cy + theta.sin() * 0.5));
            }
            ring.push(ring[0]);
            FeatureGeom {
                id: k as u64,
                bbox: [
                    (cx - 0.5) as f32,
                    (cy - 0.5) as f32,
                    (cx + 0.5) as f32,
                    (cy + 0.5) as f32,
                ],
                geom: GeomKind::Polygon(vec![ring]),
            }
        })
        .collect();
    let payload = encode_geometry_payload(&features).unwrap();

    for &target in &[10usize, 100, 1000, 10_000] {
        let q = query_for_hits(n, target);
        let actual = {
            let it = iter_feature_index(&payload).unwrap();
            it.filter_map(Result::ok)
                .filter(|e| e.bbox[0] <= q[2] && e.bbox[1] <= q[3] && e.bbox[2] >= q[0] && e.bbox[3] >= q[1])
                .count()
        };
        group.throughput(Throughput::Elements(actual as u64));
        let label = format!("n_{n}_sel_{target}_actual_{actual}");
        group.bench_function(label, |b| {
            let mut out = Vec::with_capacity(actual + 16);
            b.iter(|| {
                out.clear();
                let q = black_box(q);
                let it = iter_feature_index(&payload).unwrap();
                for e in it.flatten() {
                    if e.bbox[0] <= q[2] && e.bbox[1] <= q[3] && e.bbox[2] >= q[0] && e.bbox[3] >= q[1] {
                        out.push(e.id as u32);
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
    items.push((items.len() as u32, [0.0, extent * 0.5, extent, extent * 0.5 + 1.0]));
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

/// the gate bench: combined per-layer feature-prep at ~500 hits over 40k pages.
///
/// per query:
///   spatial_index.query_visit
///     -> for each hit idx: lookup FeatureIndexEntry by position
///     -> decode_one_geom against the geometry payload's coord area
///     -> binary-search a sorted (feature_id, class_index) array
///
/// target: ≤ 2 ms warm. fail → format reconsideration (lazarus bailout #2).
fn bench_feature_prep_combined(c: &mut Criterion) {
    let mut group = c.benchmark_group("spatial_index_feature_prep_combined");
    let n = SMALL;
    let ring_len = 16;

    let features: Vec<FeatureGeom> = (0..n)
        .map(|k| {
            let i = (k as f64) % ((n as f64).sqrt().ceil());
            let j = (k as f64) / ((n as f64).sqrt().ceil());
            let cx = i * f64::from(SPACING);
            let cy = j * f64::from(SPACING);
            let mut ring = Vec::with_capacity(ring_len + 1);
            for v in 0..ring_len {
                let theta = (v as f64) * std::f64::consts::TAU / (ring_len as f64);
                ring.push((cx + theta.cos() * 0.5, cy + theta.sin() * 0.5));
            }
            ring.push(ring[0]);
            FeatureGeom {
                id: k as u64,
                bbox: [
                    (cx - 0.5) as f32,
                    (cy - 0.5) as f32,
                    (cx + 0.5) as f32,
                    (cy + 0.5) as f32,
                ],
                geom: GeomKind::Polygon(vec![ring]),
            }
        })
        .collect();

    let payload = encode_geometry_payload(&features).unwrap();
    let entries: Vec<FeatureIndexEntry> = {
        let it = iter_feature_index(&payload).unwrap();
        let mut v = Vec::with_capacity(it.len());
        for e in it {
            v.push(e.unwrap());
        }
        v
    };
    let coord_area = {
        let it = iter_feature_index(&payload).unwrap();
        Bytes::copy_from_slice(it.coord_area())
    };

    // sidecar-style class lookup: sorted by feature_id, binary-searchable.
    let class_lookup: Vec<(u64, u16)> = (0..n).map(|k| (k as u64, (k % 17) as u16)).collect();

    let items: Vec<(u32, [f32; 4])> = entries.iter().enumerate().map(|(i, e)| (i as u32, e.bbox)).collect();
    let idx = SpatialIndex::open(build_index(&items)).unwrap();

    let q = query_for_hits(n, 500);
    let mut hits = Vec::new();
    idx.query(q, &mut hits);
    let actual = hits.len();
    group.throughput(Throughput::Elements(actual as u64));
    let label = format!("n_{n}_sel_500_actual_{actual}");
    group.bench_function(label, |b| {
        b.iter(|| {
            let mut acc: u64 = 0;
            idx.query_visit(black_box(q), |i| {
                let entry = &entries[i as usize];
                let geom = decode_one_geom(&coord_area, entry).unwrap();
                let class = class_lookup
                    .binary_search_by_key(&entry.id, |&(fid, _)| fid)
                    .map(|p| u64::from(class_lookup[p].1))
                    .unwrap_or(0);
                acc = acc.wrapping_add(class);
                if let GeomKind::Polygon(rings) = &geom
                    && let Some(r) = rings.first()
                {
                    acc = acc.wrapping_add(r.len() as u64);
                }
            });
            black_box(acc);
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_build,
    bench_query,
    bench_brute_force_baseline,
    bench_geometry_payload_linear_walk,
    bench_query_dilated_bbox,
    bench_feature_prep_combined,
);
criterion_main!(benches);

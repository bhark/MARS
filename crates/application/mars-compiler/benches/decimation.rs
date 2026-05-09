//! decimation throughput bench. measures `decimate::simplify` per row at
//! three tolerance levels against a synthetic 100k-row vec of mid-vertex-
//! count linestrings (a forvaltning2-class building outline shape).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use mars_artifact::{FeatureGeom, GeomKind};
use mars_compiler::decimate::simplify_naive;

const ROWS: usize = 100_000;
const VERTEX_COUNT: usize = 24;

fn build_features() -> Vec<FeatureGeom> {
    (0..ROWS)
        .map(|i| {
            let phase = i as f64 * 0.001;
            let coords: Vec<(f64, f64)> = (0..VERTEX_COUNT)
                .map(|j| {
                    let t = j as f64 / (VERTEX_COUNT - 1) as f64;
                    let x = t * 100.0 + phase;
                    let y = (t * std::f64::consts::TAU).sin() * 50.0;
                    (x, y)
                })
                .collect();
            let mut min_x = f32::INFINITY;
            let mut min_y = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            for &(x, y) in &coords {
                min_x = min_x.min(x as f32);
                min_y = min_y.min(y as f32);
                max_x = max_x.max(x as f32);
                max_y = max_y.max(y as f32);
            }
            FeatureGeom {
                id: i as u64,
                bbox: [min_x, min_y, max_x, max_y],
                geom: GeomKind::LineString(coords),
            }
        })
        .collect()
}

fn bench_decimation(c: &mut Criterion) {
    let features = build_features();
    let mut group = c.benchmark_group("decimation");
    group.throughput(Throughput::Elements(ROWS as u64));
    for tol in [1.0_f64, 5.0, 25.0] {
        group.bench_function(format!("tol={tol}"), |b| {
            b.iter(|| {
                for f in &features {
                    black_box(simplify_naive(&f.geom, tol));
                }
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_decimation);
criterion_main!(benches);

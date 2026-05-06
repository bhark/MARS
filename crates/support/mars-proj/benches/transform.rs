//! cost of cold Transformer construction (proj_create_crs_to_crs +
//! proj_normalize_for_visualization) and the hot transform paths
//! (transform_bbox densified, transform_points batched).
//!
//! the runtime caches transformers per thread, so runtime-level reproject
//! benches see warm-cache numbers only. this bench isolates what the cache
//! is amortising so the gap between cached and uncached is measurable.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mars_proj::{Transformer, TransformerOptions};
use mars_types::{Bbox, CrsCode};

// each pair carries its own input bbox in the source CRS so the values are
// well-conditioned for that projection. denmark in utm-32, denmark in lat/lon,
// denmark in web-mercator metres.
const PAIRS: &[(&str, &str, &str, Bbox)] = &[
    (
        "25832_to_3857",
        "EPSG:25832",
        "EPSG:3857",
        Bbox {
            min_x: 440_000.0,
            min_y: 6_050_000.0,
            max_x: 900_000.0,
            max_y: 6_400_000.0,
        },
    ),
    (
        "25832_to_4326",
        "EPSG:25832",
        "EPSG:4326",
        Bbox {
            min_x: 440_000.0,
            min_y: 6_050_000.0,
            max_x: 900_000.0,
            max_y: 6_400_000.0,
        },
    ),
    (
        "4326_to_3857",
        "EPSG:4326",
        "EPSG:3857",
        Bbox {
            min_x: 8.0,
            min_y: 54.0,
            max_x: 15.0,
            max_y: 58.0,
        },
    ),
];

const BATCH_SIZES: &[usize] = &[64, 1024, 16_384];

fn denmark_utm32_batch(n: usize) -> Vec<[f64; 2]> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t = (i as f64) / (n as f64);
        let x = 440_000.0 + (900_000.0 - 440_000.0) * t;
        let y = 6_050_000.0 + (6_400_000.0 - 6_050_000.0) * (t * 0.7).fract();
        out.push([x, y]);
    }
    out
}

fn bench_transformer_new(c: &mut Criterion) {
    let mut group = c.benchmark_group("proj_transformer_new");
    for (name, from, to, _) in PAIRS {
        let from_c = CrsCode::new(*from);
        let to_c = CrsCode::new(*to);
        group.bench_with_input(BenchmarkId::from_parameter(name), &(from_c, to_c), |b, (f, t)| {
            b.iter(|| {
                let tx = Transformer::new(f, t).unwrap();
                black_box(tx)
            });
        });
    }
    group.finish();
}

fn bench_transform_bbox(c: &mut Criterion) {
    let mut group = c.benchmark_group("proj_transform_bbox");
    for (name, from, to, bbox) in PAIRS {
        let tx = Transformer::with_options(
            &CrsCode::new(*from),
            &CrsCode::new(*to),
            TransformerOptions { densify_segments: 10 },
        )
        .unwrap();
        let bbox = *bbox;
        group.bench_with_input(BenchmarkId::from_parameter(name), &tx, |b, tx| {
            b.iter(|| {
                let out = tx.transform_bbox(bbox).unwrap();
                black_box(out)
            });
        });
    }
    group.finish();
}

fn bench_transform_points(c: &mut Criterion) {
    let tx = Transformer::new(&CrsCode::new("EPSG:25832"), &CrsCode::new("EPSG:3857")).unwrap();
    let mut group = c.benchmark_group("proj_transform_points");
    for &n in BATCH_SIZES {
        let template = denmark_utm32_batch(n);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &template, |b, template| {
            b.iter_batched_ref(
                || template.clone(),
                |buf| {
                    tx.transform_points(buf).unwrap();
                    black_box(buf.len())
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_transformer_new,
    bench_transform_bbox,
    bench_transform_points
);
criterion_main!(benches);

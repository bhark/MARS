//! WKB decode + artifact write across a synthetic polygon corpus. mirrors the
//! per-cell cost the compiler pays during snapshot rebuilds.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use mars_artifact::{ArtifactKind, ArtifactWriter, FeatureGeom, encode_geometry_payload};
use mars_compiler::wkb::decode_feature;
use mars_types::Bbox;

const FEATURES: u64 = 1024;
const RING_LEN: usize = 64;

fn polygon_wkb(id: u64) -> Vec<u8> {
    let cx = (id as f64) * 12.0;
    let cy = ((id % 31) as f64) * 7.0;
    // WKB: little-endian, type=3 (polygon), 1 ring, RING_LEN+1 points.
    let mut v = Vec::with_capacity(9 + 4 + 8 + 8 + 16 * (RING_LEN + 1));
    v.push(1u8);
    v.extend_from_slice(&3u32.to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&((RING_LEN + 1) as u32).to_le_bytes());
    let first_x = cx + 5.0;
    let first_y = cy;
    v.extend_from_slice(&first_x.to_le_bytes());
    v.extend_from_slice(&first_y.to_le_bytes());
    for i in 1..RING_LEN {
        let theta = (i as f64) * std::f64::consts::TAU / (RING_LEN as f64);
        let x = cx + theta.cos() * 5.0;
        let y = cy + theta.sin() * 5.0;
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
    }
    v.extend_from_slice(&first_x.to_le_bytes());
    v.extend_from_slice(&first_y.to_le_bytes());
    v
}

fn bench_decode_wkb(c: &mut Criterion) {
    let corpus: Vec<(u64, Vec<u8>)> = (0..FEATURES).map(|id| (id, polygon_wkb(id))).collect();

    let mut group = c.benchmark_group("compiler_decode");
    group.throughput(Throughput::Elements(FEATURES));
    group.bench_function("wkb_to_feature", |b| {
        b.iter(|| {
            let mut acc = 0u64;
            for (id, wkb) in &corpus {
                let feat = decode_feature(*id, wkb, None).unwrap();
                acc = acc.wrapping_add(feat.id);
            }
            black_box(acc)
        });
    });
    group.finish();
}

fn bench_encode_payload(c: &mut Criterion) {
    let features: Vec<FeatureGeom> = (0..FEATURES)
        .map(|id| {
            let wkb = polygon_wkb(id);
            decode_feature(id, &wkb, None).unwrap()
        })
        .collect();

    let mut group = c.benchmark_group("compiler_decode");
    group.throughput(Throughput::Elements(FEATURES));
    group.bench_function("encode_geometry_payload", |b| {
        b.iter(|| {
            let bytes = encode_geometry_payload(&features).unwrap();
            black_box(bytes.len())
        });
    });
    group.finish();
}

fn bench_decode_then_write(c: &mut Criterion) {
    let corpus: Vec<(u64, Vec<u8>)> = (0..FEATURES).map(|id| (id, polygon_wkb(id))).collect();

    let mut group = c.benchmark_group("compiler_decode");
    group.throughput(Throughput::Elements(FEATURES));
    group.bench_function("decode_then_artifact_write", |b| {
        b.iter(|| {
            let features: Vec<FeatureGeom> = corpus
                .iter()
                .map(|(id, wkb)| decode_feature(*id, wkb, None).unwrap())
                .collect();
            let mut w = ArtifactWriter::new(ArtifactKind::Source);
            let n = features.len() as u64;
            w.add_geometry_payload(features)
                .set_bbox(Bbox::new(-1.0e6, -1.0e6, 1.0e6, 1.0e6))
                .set_feature_count(n);
            let out = w.finish().unwrap();
            black_box(out.len())
        });
    });
    group.finish();
}

criterion_group!(benches, bench_decode_wkb, bench_encode_payload, bench_decode_then_write);
criterion_main!(benches);

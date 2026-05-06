//! WKB decode + artifact write across synthetic polygon, line and point
//! corpora. mirrors the per-cell cost the compiler pays during snapshot
//! rebuilds.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mars_artifact::{
    ArtifactKind, ArtifactWriter, FeatureGeom, GeomPayloadBuilder, SectionKind, encode_geometry_payload,
};
use mars_compiler::wkb::{decode_feature, write_into};
use mars_types::Bbox;

const FEATURES: u64 = 1024;
const RING_LEN: usize = 64;
const LINE_LEN: usize = 128;
const DENSE_RING_LEN: usize = 8192;

fn polygon_wkb(id: u64, ring_len: usize) -> Vec<u8> {
    let cx = (id as f64) * 12.0;
    let cy = ((id % 31) as f64) * 7.0;
    // WKB: little-endian, type=3 (polygon), 1 ring, ring_len+1 points.
    let mut v = Vec::with_capacity(9 + 4 + 8 + 8 + 16 * (ring_len + 1));
    v.push(1u8);
    v.extend_from_slice(&3u32.to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&((ring_len + 1) as u32).to_le_bytes());
    let first_x = cx + 5.0;
    let first_y = cy;
    v.extend_from_slice(&first_x.to_le_bytes());
    v.extend_from_slice(&first_y.to_le_bytes());
    for i in 1..ring_len {
        let theta = (i as f64) * std::f64::consts::TAU / (ring_len as f64);
        let x = cx + theta.cos() * 5.0;
        let y = cy + theta.sin() * 5.0;
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
    }
    v.extend_from_slice(&first_x.to_le_bytes());
    v.extend_from_slice(&first_y.to_le_bytes());
    v
}

fn linestring_wkb(id: u64) -> Vec<u8> {
    let cx = (id as f64) * 12.0;
    let cy = ((id % 31) as f64) * 7.0;
    let mut v = Vec::with_capacity(9 + 4 + 16 * LINE_LEN);
    v.push(1u8);
    v.extend_from_slice(&2u32.to_le_bytes());
    v.extend_from_slice(&(LINE_LEN as u32).to_le_bytes());
    for i in 0..LINE_LEN {
        let t = i as f64;
        let x = cx + t * 0.5;
        let y = cy + (t * 0.25).sin() * 3.0;
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
    }
    v
}

fn point_wkb(id: u64) -> Vec<u8> {
    let x = (id as f64) * 11.0;
    let y = ((id % 53) as f64) * 9.0;
    let mut v = Vec::with_capacity(21);
    v.push(1u8);
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&x.to_le_bytes());
    v.extend_from_slice(&y.to_le_bytes());
    v
}

fn bench_decode_wkb(c: &mut Criterion) {
    let mut group = c.benchmark_group("compiler_decode_wkb");
    group.throughput(Throughput::Elements(FEATURES));

    for (name, builder) in [
        ("polygon_1024", polygon_corpus as fn() -> Vec<(u64, Vec<u8>)>),
        ("linestring_1024", linestring_corpus),
        ("point_1024", point_corpus),
    ] {
        let corpus = builder();
        group.bench_with_input(BenchmarkId::from_parameter(name), &corpus, |b, corpus| {
            b.iter(|| {
                let mut acc = 0u64;
                for (id, wkb) in corpus {
                    let feat = decode_feature(*id, wkb, None).unwrap();
                    acc = acc.wrapping_add(feat.id);
                }
                black_box(acc)
            });
        });
    }
    group.finish();
}

fn bench_decode_dense_polygon(c: &mut Criterion) {
    // single dense polygon, isolates per-vertex decode cost from per-feature
    // dispatch overhead.
    let wkb = polygon_wkb(0, DENSE_RING_LEN);

    let mut group = c.benchmark_group("compiler_decode_wkb");
    group.throughput(Throughput::Elements(DENSE_RING_LEN as u64));
    group.bench_function("polygon_dense_8192_vertices", |b| {
        b.iter(|| {
            let feat = decode_feature(0, &wkb, None).unwrap();
            black_box(feat.id)
        });
    });
    group.finish();
}

fn bench_encode_payload(c: &mut Criterion) {
    let features: Vec<FeatureGeom> = (0..FEATURES)
        .map(|id| {
            let wkb = polygon_wkb(id, RING_LEN);
            decode_feature(id, &wkb, None).unwrap()
        })
        .collect();

    let mut group = c.benchmark_group("compiler_decode_wkb");
    group.throughput(Throughput::Elements(FEATURES));
    group.bench_function("encode_geometry_payload/polygon", |b| {
        b.iter(|| {
            let bytes = encode_geometry_payload(&features).unwrap();
            black_box(bytes.len())
        });
    });
    group.finish();
}

fn bench_decode_then_write(c: &mut Criterion) {
    let corpus: Vec<(u64, Vec<u8>)> = (0..FEATURES).map(|id| (id, polygon_wkb(id, RING_LEN))).collect();

    let mut group = c.benchmark_group("compiler_decode_wkb");
    group.throughput(Throughput::Elements(FEATURES));
    group.bench_function("decode_then_artifact_write/polygon", |b| {
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

fn bench_streaming_then_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("compiler_decode_wkb");
    group.throughput(Throughput::Elements(FEATURES));

    for (name, builder) in [
        ("polygon", polygon_corpus as fn() -> Vec<(u64, Vec<u8>)>),
        ("linestring", linestring_corpus),
    ] {
        let corpus = builder();
        let bench_name = format!("streaming_wkb_to_artifact/{name}");
        group.bench_with_input(
            BenchmarkId::new("streaming_wkb_to_artifact", name),
            &corpus,
            |b, corpus| {
                b.iter(|| {
                    let mut geom = GeomPayloadBuilder::new();
                    for (id, wkb) in corpus {
                        write_into(&mut geom, *id, wkb, None).unwrap();
                    }
                    let geom_bytes = geom.finish().unwrap();
                    let mut w = ArtifactWriter::new(ArtifactKind::Source);
                    w.add_section(SectionKind::GeometryPayload, geom_bytes);
                    w.set_bbox(Bbox::new(-1.0e6, -1.0e6, 1.0e6, 1.0e6));
                    w.set_feature_count(FEATURES);
                    let out = w.finish().unwrap();
                    black_box(out.len())
                });
            },
        );
        let _ = bench_name; // kept for grep-ability across bench output
    }
    group.finish();
}

fn polygon_corpus() -> Vec<(u64, Vec<u8>)> {
    (0..FEATURES).map(|id| (id, polygon_wkb(id, RING_LEN))).collect()
}

fn linestring_corpus() -> Vec<(u64, Vec<u8>)> {
    (0..FEATURES).map(|id| (id, linestring_wkb(id))).collect()
}

fn point_corpus() -> Vec<(u64, Vec<u8>)> {
    (0..FEATURES).map(|id| (id, point_wkb(id))).collect()
}

criterion_group!(
    benches,
    bench_decode_wkb,
    bench_decode_dense_polygon,
    bench_encode_payload,
    bench_decode_then_write,
    bench_streaming_then_write,
);
criterion_main!(benches);

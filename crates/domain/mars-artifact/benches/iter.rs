//! geometry index iteration + per-feature coord decode over a synthetic
//! polygon-heavy artifact. measures the inner loop runtime exercises on every
//! cell render.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;
use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use mars_artifact::{
    ArtifactKind, ArtifactWriter, FeatureGeom, GeomKind, SectionKind, decode_one_geom, iter_feature_index,
};
use mars_types::Bbox;

const FEATURES: u64 = 1024;
const RING_LEN: usize = 64;

fn make_polygon(id: u64) -> FeatureGeom {
    let cx = (id as f64) * 12.0;
    let cy = ((id % 31) as f64) * 7.0;
    let mut ring = Vec::with_capacity(RING_LEN + 1);
    for i in 0..RING_LEN {
        let theta = (i as f64) * std::f64::consts::TAU / (RING_LEN as f64);
        ring.push((cx + theta.cos() * 5.0, cy + theta.sin() * 5.0));
    }
    ring.push(ring[0]);
    let bbox = [
        (cx - 5.0) as f32,
        (cy - 5.0) as f32,
        (cx + 5.0) as f32,
        (cy + 5.0) as f32,
    ];
    FeatureGeom {
        id,
        bbox,
        geom: GeomKind::Polygon(vec![ring]),
    }
}

fn build_source_artifact(features: &[FeatureGeom]) -> Bytes {
    let mut w = ArtifactWriter::new(ArtifactKind::Source);
    let n = features.len() as u64;
    w.add_geometry_payload(features.to_vec())
        .set_bbox(Bbox::new(-1.0e6, -1.0e6, 1.0e6, 1.0e6))
        .set_feature_count(n);
    w.finish().unwrap()
}

fn bench_iter_index_only(c: &mut Criterion) {
    let features: Vec<FeatureGeom> = (0..FEATURES).map(make_polygon).collect();
    let bytes = build_source_artifact(&features);
    let reader = mars_artifact::ArtifactReader::open(bytes).unwrap();
    let section = reader.section(SectionKind::GeometryPayload).unwrap();

    let mut group = c.benchmark_group("artifact_iter");
    group.throughput(Throughput::Elements(FEATURES));
    group.bench_function("index_only", |b| {
        b.iter(|| {
            let iter = iter_feature_index(&section).unwrap();
            let mut acc = 0u64;
            for entry in iter {
                let entry = entry.unwrap();
                acc = acc.wrapping_add(entry.id);
            }
            black_box(acc)
        });
    });
    group.finish();
}

fn bench_decode_all(c: &mut Criterion) {
    let features: Vec<FeatureGeom> = (0..FEATURES).map(make_polygon).collect();
    let bytes = build_source_artifact(&features);
    let reader = mars_artifact::ArtifactReader::open(bytes).unwrap();
    let section = reader.section(SectionKind::GeometryPayload).unwrap();

    let mut group = c.benchmark_group("artifact_iter");
    group.throughput(Throughput::Elements(FEATURES));
    group.bench_function("decode_all", |b| {
        b.iter(|| {
            let iter = iter_feature_index(&section).unwrap();
            let coord_area = iter.coord_area();
            let mut points = 0usize;
            for entry in iter {
                let entry = entry.unwrap();
                let geom = decode_one_geom(coord_area, &entry).unwrap();
                if let GeomKind::Polygon(rs) = geom {
                    for r in &rs {
                        points += r.len();
                    }
                }
            }
            black_box(points)
        });
    });
    group.finish();
}

criterion_group!(benches, bench_iter_index_only, bench_decode_all);
criterion_main!(benches);

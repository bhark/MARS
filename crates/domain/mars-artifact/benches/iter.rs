//! geometry index iteration + per-feature coord decode over synthetic
//! polygon, line and mixed-geom artifacts. measures the inner loop runtime
//! exercises on every cell render.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;
use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mars_artifact::{
    ArtifactKind, ArtifactWriter, FeatureGeom, GeomKind, SectionKind, decode_one_geom, iter_feature_index,
};
use mars_types::Bbox;

const FEATURES: u64 = 1024;
const RING_LEN: usize = 64;
const LINE_LEN: usize = 64;

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
        user_id: id,
        bbox,
        geom: GeomKind::Polygon(vec![ring]),
    }
}

fn make_linestring(id: u64) -> FeatureGeom {
    let cx = (id as f64) * 12.0;
    let cy = ((id % 31) as f64) * 7.0;
    let mut verts = Vec::with_capacity(LINE_LEN);
    for i in 0..LINE_LEN {
        let t = i as f64;
        verts.push((cx + t * 0.5, cy + (t * 0.25).sin() * 3.0));
    }
    let (mut lo_x, mut lo_y, mut hi_x, mut hi_y) = (f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);
    for &(x, y) in &verts {
        lo_x = lo_x.min(x as f32);
        lo_y = lo_y.min(y as f32);
        hi_x = hi_x.max(x as f32);
        hi_y = hi_y.max(y as f32);
    }
    FeatureGeom {
        user_id: id,
        bbox: [lo_x, lo_y, hi_x, hi_y],
        geom: GeomKind::LineString(verts),
    }
}

fn make_point(id: u64) -> FeatureGeom {
    let x = (id as f64) * 11.0;
    let y = ((id % 53) as f64) * 9.0;
    FeatureGeom {
        user_id: id,
        bbox: [x as f32, y as f32, x as f32, y as f32],
        geom: GeomKind::Point((x, y)),
    }
}

fn make_mixed(id: u64) -> FeatureGeom {
    // round-robin across polygon, line, point so the dispatch path sees
    // alternating geom types.
    match id % 3 {
        0 => make_polygon(id),
        1 => make_linestring(id),
        _ => make_point(id),
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

fn bench_index_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("artifact_index_only");
    group.throughput(Throughput::Elements(FEATURES));

    for (name, builder) in [
        ("polygon", make_polygon as fn(u64) -> FeatureGeom),
        ("linestring", make_linestring as fn(u64) -> FeatureGeom),
    ] {
        let features: Vec<FeatureGeom> = (0..FEATURES).map(builder).collect();
        let bytes = build_source_artifact(&features);
        let reader = mars_artifact::ArtifactReader::open(bytes).unwrap();
        let section = reader.section(SectionKind::GeometryPayload).unwrap();

        group.bench_with_input(BenchmarkId::from_parameter(name), &section, |b, section| {
            b.iter(|| {
                let iter = iter_feature_index(section).unwrap();
                let mut acc = 0u64;
                for entry in iter {
                    let entry = entry.unwrap();
                    acc = acc.wrapping_add(entry.user_id);
                }
                black_box(acc)
            });
        });
    }
    group.finish();
}

fn bench_decode_all(c: &mut Criterion) {
    let mut group = c.benchmark_group("artifact_decode_all");
    group.throughput(Throughput::Elements(FEATURES));

    for (name, builder) in [
        ("polygon", make_polygon as fn(u64) -> FeatureGeom),
        ("linestring", make_linestring as fn(u64) -> FeatureGeom),
        ("mixed", make_mixed as fn(u64) -> FeatureGeom),
    ] {
        let features: Vec<FeatureGeom> = (0..FEATURES).map(builder).collect();
        let bytes = build_source_artifact(&features);
        let reader = mars_artifact::ArtifactReader::open(bytes).unwrap();
        let section = reader.section(SectionKind::GeometryPayload).unwrap();

        group.bench_with_input(BenchmarkId::from_parameter(name), &section, |b, section| {
            b.iter(|| {
                let iter = iter_feature_index(section).unwrap();
                let coord_area = iter.coord_area();
                let mut points = 0usize;
                for entry in iter {
                    let entry = entry.unwrap();
                    let geom = decode_one_geom(coord_area, &entry).unwrap();
                    match geom {
                        GeomKind::Polygon(rs) => {
                            for r in &rs {
                                points += r.len();
                            }
                        }
                        GeomKind::LineString(v) => points += v.len(),
                        GeomKind::Point(_) => points += 1,
                        _ => {}
                    }
                }
                black_box(points)
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_index_only, bench_decode_all);
criterion_main!(benches);

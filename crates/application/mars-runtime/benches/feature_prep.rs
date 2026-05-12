//! runtime feature-prep bench. drives the per-page render loop end-to-end
//! against a synthesised page artifact + class sidecar pair, mirroring the
//! work `render::decode_page_to_ops` does on a real request:
//!
//!   ArtifactReader::open(page)
//!     -> SpatialIndex::open + query        (viewport hit-test)
//!     -> decode_geometry_at_slots          (varint geom decode)
//!     -> [optional] reproject features     (per-feature mars_proj)
//!   ArtifactReader::open(class_sidecar)
//!     -> decode_class_assignment + decode_style_refs
//!     -> per-feature binary search + Stylesheet lookup
//!     -> DrawOp::Path build (subpath rasterisation in pixel space)
//!
//! `spatial_index_feature_prep_combined` benches the
//! artifact-internal subset; this is the runtime-level superset, including
//! the artifact envelope reads and the sidecar join. gate (informational):
//! ≤ 2 ms warm at 40k features × 500 hits, same-CRS.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::hint::black_box;
use std::sync::Arc;

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mars_artifact::{
    ArtifactKind, ArtifactReader, ArtifactWriter, DEFAULT_NODE_SIZE, FeatureGeom, GeomKind, SectionKind, SpatialIndex,
    SpatialIndexBuilder, decode_class_assignment, decode_style_refs, encode_geometry_payload,
};
use mars_render_port::{DrawOp, Path, Subpath};
use mars_style::{Colour, FillPaint, Style, Stylesheet};
use mars_types::{Bbox, CrsCode};

const SMALL: usize = 40_000;
const LARGE: usize = 200_000;
const SPACING: f64 = 5.0; // metres between feature centres
const ORIGIN_X: f64 = 700_000.0; // realistic UTM zone 32N offset (Copenhagen-ish)
const ORIGIN_Y: f64 = 6_175_000.0;
const NUM_CLASSES: usize = 8;

fn make_features(n: usize) -> Vec<FeatureGeom> {
    let side = (n as f64).sqrt().ceil() as usize;
    let ring_len = 16;
    (0..n)
        .map(|k| {
            let i = (k % side) as f64;
            let j = (k / side) as f64;
            let cx = ORIGIN_X + i * SPACING;
            let cy = ORIGIN_Y + j * SPACING;
            let mut ring = Vec::with_capacity(ring_len + 1);
            for v in 0..ring_len {
                let theta = (v as f64) * std::f64::consts::TAU / (ring_len as f64);
                ring.push((cx + theta.cos() * 0.5, cy + theta.sin() * 0.5));
            }
            ring.push(ring[0]);
            FeatureGeom {
                user_id: k as u64,
                bbox: [
                    (cx - 0.5) as f32,
                    (cy - 0.5) as f32,
                    (cx + 0.5) as f32,
                    (cy + 0.5) as f32,
                ],
                geom: GeomKind::Polygon(vec![ring]),
            }
        })
        .collect()
}

fn page_extent(n: usize) -> f64 {
    (n as f64).sqrt().ceil() * SPACING
}

fn page_bbox(n: usize) -> Bbox {
    let extent = page_extent(n);
    Bbox::new(
        ORIGIN_X - 1.0,
        ORIGIN_Y - 1.0,
        ORIGIN_X + extent + 1.0,
        ORIGIN_Y + extent + 1.0,
    )
}

/// query bbox sized so the area ≈ target_hits feature centres.
fn query_for_hits(n: usize, target_hits: usize) -> Bbox {
    let extent = page_extent(n);
    let area = (target_hits as f64) * SPACING * SPACING;
    let half = (area.sqrt() / 2.0).max(1.0);
    let cx = ORIGIN_X + extent * 0.5;
    let cy = ORIGIN_Y + extent * 0.5;
    Bbox::new(cx - half, cy - half, cx + half, cy + half)
}

fn bbox_to_f32(b: Bbox) -> [f32; 4] {
    [b.min_x as f32, b.min_y as f32, b.max_x as f32, b.max_y as f32]
}

fn build_page(features: &[FeatureGeom]) -> Bytes {
    let mut idx_builder = SpatialIndexBuilder::new(DEFAULT_NODE_SIZE).unwrap();
    for (i, f) in features.iter().enumerate() {
        idx_builder.add(i as u32, f.bbox);
    }
    let idx_bytes = idx_builder.finish().unwrap();
    let geom_bytes = encode_geometry_payload(features).unwrap();

    let n = features.len();
    let mut w = ArtifactWriter::new(ArtifactKind::Source);
    w.set_bbox(page_bbox(n))
        .set_feature_count(n as u64)
        .add_section(SectionKind::SpatialIndex, idx_bytes)
        .add_section(SectionKind::GeometryPayload, geom_bytes);
    w.finish().unwrap()
}

fn build_class_sidecar(n: usize) -> Bytes {
    // sorted-by-feature_idx (slot, class_index). matches snapshot output.
    let assignments: Vec<(u32, u16)> = (0..n).map(|k| (k as u32, (k % NUM_CLASSES) as u16)).collect();
    let style_refs: Vec<String> = (0..NUM_CLASSES).map(|c| format!("style_{c}")).collect();
    let mut w = ArtifactWriter::new(ArtifactKind::Layer);
    w.set_bbox(page_bbox(n))
        .add_class_assignment(&assignments)
        .add_style_refs(&style_refs);
    w.finish().unwrap()
}

fn build_stylesheet() -> Stylesheet {
    let mut geometry: BTreeMap<String, Arc<Style>> = BTreeMap::new();
    for c in 0..NUM_CLASSES {
        let s = Style {
            fill: Some(FillPaint::Solid(Colour::rgba(20 + c as u8 * 28, 100, 200, 220))),
            stroke: Some(Colour::rgba(10, 30, 80, 255)),
            stroke_width: Some(1.0),
            stroke_dasharray: None,
            stroke_linecap: None,
            stroke_linejoin: None,
            marker: None,
        };
        geometry.insert(format!("style_{c}"), Arc::new(s));
    }
    Stylesheet {
        geometry,
        labels: BTreeMap::new(),
    }
}

fn fallback_style() -> Arc<Style> {
    Arc::new(Style {
        fill: Some(FillPaint::Solid(Colour::rgba(64, 128, 220, 200))),
        stroke: Some(Colour::rgba(32, 64, 110, 255)),
        stroke_width: Some(1.0),
        stroke_dasharray: None,
        stroke_linecap: None,
        stroke_linejoin: None,
        marker: None,
    })
}

const CANVAS_W: u32 = 512;
const CANVAS_H: u32 = 512;

fn world_to_pixel(c: (f64, f64), v: Bbox) -> (f32, f32) {
    let nx = (c.0 - v.min_x) / v.width();
    let ny = (c.1 - v.min_y) / v.height();
    let px = nx * f64::from(CANVAS_W);
    let py = (1.0 - ny) * f64::from(CANVAS_H);
    (px as f32, py as f32)
}

fn polygon_to_drawop(rings: &[Vec<(f64, f64)>], v: Bbox, style: Arc<Style>) -> DrawOp {
    let subpaths = rings
        .iter()
        .map(|r| Subpath {
            points: r.iter().map(|&c| world_to_pixel(c, v)).collect(),
            closed: true,
        })
        .collect();
    DrawOp::Path {
        path: Path { subpaths },
        style,
    }
}

/// resolves feature_idx → style ref name. mirrors `runtime::render::ClassResolver`
/// exactly without depending on its private symbol.
struct ClassJoin {
    by_slot: Vec<Option<u16>>,
    style_refs: Vec<String>,
}

impl ClassJoin {
    fn open(bytes: Bytes, page_feature_count: usize) -> Self {
        let reader = ArtifactReader::open(bytes).unwrap();
        let class_bytes = reader.section(SectionKind::ClassAssignment).unwrap();
        let style_refs_bytes = reader.section(SectionKind::StyleRefs).unwrap();
        let assignments = decode_class_assignment(&class_bytes).unwrap();
        let style_refs = decode_style_refs(&style_refs_bytes).unwrap();
        let mut by_slot: Vec<Option<u16>> = vec![None; page_feature_count];
        for (slot, cls) in assignments {
            let s = slot as usize;
            if s < by_slot.len() {
                by_slot[s] = Some(cls);
            }
        }
        Self { by_slot, style_refs }
    }

    fn style_ref_for(&self, feature_idx: u32) -> Option<&str> {
        let cls = (*self.by_slot.get(feature_idx as usize)?)? as usize;
        self.style_refs.get(cls).map(String::as_str)
    }
}

#[allow(clippy::too_many_arguments)]
fn feature_prep_once(
    page_bytes: Bytes,
    class_bytes: Bytes,
    qbb: Bbox,
    viewport: Bbox,
    stylesheet: &Stylesheet,
    fallback: &Arc<Style>,
    transformer: Option<&mars_proj::Transformer>,
    slot_buf: &mut Vec<u32>,
) -> Vec<DrawOp> {
    let reader = ArtifactReader::open(page_bytes).unwrap();
    let spatial = reader.section(SectionKind::SpatialIndex).unwrap();
    let geom = reader.section(SectionKind::GeometryPayload).unwrap();
    let idx = SpatialIndex::open(spatial).unwrap();
    let page_feature_count = idx.len() as usize;
    slot_buf.clear();
    idx.query(bbox_to_f32(qbb), slot_buf);
    if slot_buf.is_empty() {
        return Vec::new();
    }
    let mut sorted_slots = slot_buf.clone();
    sorted_slots.sort_unstable();
    sorted_slots.dedup();
    // walk the index alongside the slot cursor so we keep slots paired
    // with their decoded features (decode_geometry_at_slots discards the
    // slot identity).
    let iter = mars_artifact::iter_feature_index(&geom).unwrap();
    let coord_area = iter.coord_area();
    let mut paired: Vec<(u32, FeatureGeom)> = Vec::with_capacity(sorted_slots.len());
    let mut cursor = 0usize;
    for (slot_idx, entry) in iter.enumerate() {
        if cursor >= sorted_slots.len() {
            break;
        }
        let entry = entry.unwrap();
        let slot_u32 = slot_idx as u32;
        if slot_u32 != sorted_slots[cursor] {
            continue;
        }
        cursor += 1;
        let g = mars_artifact::decode_one_geom(coord_area, &entry).unwrap();
        let g = if let Some(t) = transformer {
            project_geom(&g, t)
        } else {
            g
        };
        paired.push((
            slot_u32,
            FeatureGeom {
                user_id: entry.user_id,
                bbox: entry.bbox,
                geom: g,
            },
        ));
    }
    let class = ClassJoin::open(class_bytes, page_feature_count);
    let mut ops = Vec::with_capacity(paired.len());
    for (slot, f) in paired {
        let style = class
            .style_ref_for(slot)
            .and_then(|n| stylesheet.geometry.get(n).cloned())
            .unwrap_or_else(|| fallback.clone());
        if let GeomKind::Polygon(rings) = &f.geom {
            ops.push(polygon_to_drawop(rings, viewport, style));
        }
    }
    ops
}

fn project_geom(g: &GeomKind, t: &mars_proj::Transformer) -> GeomKind {
    match g {
        GeomKind::Polygon(rings) => {
            let mut out = Vec::with_capacity(rings.len());
            for ring in rings {
                let mut buf: Vec<[f64; 2]> = ring.iter().map(|&(x, y)| [x, y]).collect();
                t.transform_points(&mut buf).unwrap();
                out.push(buf.into_iter().map(|p| (p[0], p[1])).collect());
            }
            GeomKind::Polygon(out)
        }
        // bench corpus is polygon-only; passthrough keeps the bench honest.
        other => other.clone(),
    }
}

#[derive(Clone, Copy)]
struct Case {
    n: usize,
    target_hits: usize,
}

const CASES: &[Case] = &[
    Case {
        n: SMALL,
        target_hits: 500,
    }, // gate point
    Case {
        n: SMALL,
        target_hits: 2000,
    }, // denser viewport
    Case {
        n: LARGE,
        target_hits: 500,
    }, // cadastral large page
];

fn bench_feature_prep_same_crs(c: &mut Criterion) {
    let mut group = c.benchmark_group("runtime_feature_prep_same_crs");
    let stylesheet = build_stylesheet();
    let fallback = fallback_style();
    for &case in CASES {
        let features = make_features(case.n);
        let page = build_page(&features);
        let sidecar = build_class_sidecar(case.n);
        let viewport = page_bbox(case.n);
        let qbb = query_for_hits(case.n, case.target_hits);

        // probe actual hit count for throughput labelling.
        let mut probe_slots = Vec::new();
        {
            let r = ArtifactReader::open(page.clone()).unwrap();
            let s = r.section(SectionKind::SpatialIndex).unwrap();
            SpatialIndex::open(s).unwrap().query(bbox_to_f32(qbb), &mut probe_slots);
        }
        let actual = probe_slots.len();
        group.throughput(Throughput::Elements(actual as u64));
        let id = BenchmarkId::from_parameter(format!("n_{}_sel_{}_actual_{}", case.n, case.target_hits, actual));
        group.bench_with_input(id, &(page, sidecar), |b, (page, sidecar)| {
            let mut slots = Vec::with_capacity(actual + 16);
            b.iter(|| {
                let ops = feature_prep_once(
                    page.clone(),
                    sidecar.clone(),
                    qbb,
                    viewport,
                    &stylesheet,
                    &fallback,
                    None,
                    &mut slots,
                );
                black_box(ops);
            });
        });
    }
    group.finish();
}

fn bench_feature_prep_cross_crs(c: &mut Criterion) {
    let mut group = c.benchmark_group("runtime_feature_prep_cross_crs");
    let stylesheet = build_stylesheet();
    let fallback = fallback_style();
    let from = CrsCode::new("EPSG:25832");
    let to = CrsCode::new("EPSG:3857");
    let case = Case {
        n: SMALL,
        target_hits: 500,
    };
    let features = make_features(case.n);
    let page = build_page(&features);
    let sidecar = build_class_sidecar(case.n);
    let viewport = page_bbox(case.n);
    let qbb = query_for_hits(case.n, case.target_hits);

    let mut probe_slots = Vec::new();
    {
        let r = ArtifactReader::open(page.clone()).unwrap();
        let s = r.section(SectionKind::SpatialIndex).unwrap();
        SpatialIndex::open(s).unwrap().query(bbox_to_f32(qbb), &mut probe_slots);
    }
    let actual = probe_slots.len();
    group.throughput(Throughput::Elements(actual as u64));
    let id = BenchmarkId::from_parameter(format!("n_{}_sel_{}_actual_{}", case.n, case.target_hits, actual));
    group.bench_with_input(id, &(page, sidecar), |b, (page, sidecar)| {
        let mut slots = Vec::with_capacity(actual + 16);
        // construct transformer once outside iter to avoid measuring proj_create_crs_to_crs.
        let xform = mars_proj::cached_transformer(&from, &to).unwrap();
        b.iter(|| {
            let ops = feature_prep_once(
                page.clone(),
                sidecar.clone(),
                qbb,
                viewport,
                &stylesheet,
                &fallback,
                Some(&xform),
                &mut slots,
            );
            black_box(ops);
        });
    });
    group.finish();
}

criterion_group!(benches, bench_feature_prep_same_crs, bench_feature_prep_cross_crs);
criterion_main!(benches);

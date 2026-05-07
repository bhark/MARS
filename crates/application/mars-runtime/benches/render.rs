//! end-to-end Runtime::render against in-memory store/cache and a noop
//! renderer/encoder. exercises decode + cull + project + (optional)
//! reprojection for both polygon and linestring corpora, single- and
//! multi-layer plans.
//!
//! the noop renderer/encoder keep signal inside runtime-owned work; tiny-skia
//! draw + png/jpeg encode cost is benched separately in `mars-render`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use bytes::Bytes;
use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mars_artifact::{ArtifactKind, ArtifactWriter, FeatureGeom, GeomKind, SourceRef, compute_content_hash};
use mars_grid::BandConfig;
use mars_render_port::{Canvas, DrawOp, EncodeError, Encoder, ImageFormat, Pixmap, RenderError, Renderer};
use mars_runtime::{
    Deps, RenderPlan, Runtime,
    key::{layer_key, source_key},
    state::{LayerCellKey, LayerCellState, RuntimeState, SourceCellKey},
};
use mars_store::ObjectStore;
use mars_store::mem::{InMemoryCache, InMemoryStore};
use mars_style::{Colour, Style, Stylesheet};
use mars_types::{ArtifactEntry, Bbox, Cell, CrsCode, LayerId, Manifest, ScaleBand};
use tokio::runtime::Builder;

const COLLECTION: &str = "parcels";
const BAND: &str = "hi";
const FEATURES: u64 = 256;
const RING_LEN: usize = 32;
const LINE_LEN: usize = 32;

// canonical CRS extent: a small region inside the EPSG:25832 zone (Denmark).
// realistic coords keep proj_create_crs_to_crs / transform_bbox in their
// well-conditioned domain; synthetic (0..1024) drift to the gulf of guinea
// works numerically but is misleading when reading bench output.
const CANONICAL_MIN_X: f64 = 450_000.0;
const CANONICAL_MIN_Y: f64 = 6_100_000.0;
const CANONICAL_MAX_X: f64 = 460_000.0;
const CANONICAL_MAX_Y: f64 = 6_110_000.0;

#[derive(Clone, Copy)]
enum GeomShape {
    Polygon,
    LineString,
}

#[derive(Clone, Copy)]
struct BuildOpts {
    geom: GeomShape,
    layers: usize,
    reproject_to: Option<&'static str>,
}

#[derive(Default)]
struct NoopRenderer;

impl Renderer for NoopRenderer {
    fn render(&self, canvas: Canvas, _ops: &[DrawOp]) -> Result<Pixmap, RenderError> {
        Ok(Pixmap {
            width: canvas.width,
            height: canvas.height,
            premultiplied_rgba: vec![0u8; (canvas.width * canvas.height * 4) as usize],
        })
    }
}

#[derive(Default)]
struct NoopEncoder;

impl Encoder for NoopEncoder {
    fn encode(&self, _pixmap: &Pixmap, _format: ImageFormat) -> Result<Vec<u8>, EncodeError> {
        Ok(Vec::new())
    }
}

fn make_polygon(id: u64) -> FeatureGeom {
    let cx = CANONICAL_MIN_X + 100.0 + (id as f64) * 5.0;
    let cy = CANONICAL_MIN_Y + 100.0 + ((id % 16) as f64) * 5.0;
    let mut ring = Vec::with_capacity(RING_LEN + 1);
    for i in 0..RING_LEN {
        let theta = (i as f64) * std::f64::consts::TAU / (RING_LEN as f64);
        ring.push((cx + theta.cos() * 2.0, cy + theta.sin() * 2.0));
    }
    ring.push(ring[0]);
    let bbox = [
        (cx - 2.0) as f32,
        (cy - 2.0) as f32,
        (cx + 2.0) as f32,
        (cy + 2.0) as f32,
    ];
    FeatureGeom {
        id,
        bbox,
        geom: GeomKind::Polygon(vec![ring]),
    }
}

fn make_linestring(id: u64) -> FeatureGeom {
    let cx = CANONICAL_MIN_X + 100.0 + (id as f64) * 5.0;
    let cy = CANONICAL_MIN_Y + 100.0 + ((id % 16) as f64) * 5.0;
    let mut verts = Vec::with_capacity(LINE_LEN);
    for i in 0..LINE_LEN {
        let t = i as f64;
        verts.push((cx + t * 0.2, cy + (t * 0.4).sin() * 1.5));
    }
    let (mut lo_x, mut lo_y, mut hi_x, mut hi_y) = (f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);
    for &(x, y) in &verts {
        lo_x = lo_x.min(x as f32);
        lo_y = lo_y.min(y as f32);
        hi_x = hi_x.max(x as f32);
        hi_y = hi_y.max(y as f32);
    }
    FeatureGeom {
        id,
        bbox: [lo_x, lo_y, hi_x, hi_y],
        geom: GeomKind::LineString(verts),
    }
}

fn build_source_bytes(geom: GeomShape) -> Bytes {
    let features: Vec<FeatureGeom> = match geom {
        GeomShape::Polygon => (0..FEATURES).map(make_polygon).collect(),
        GeomShape::LineString => (0..FEATURES).map(make_linestring).collect(),
    };
    let mut w = ArtifactWriter::new(ArtifactKind::Source);
    w.add_geometry_payload(features)
        .set_bbox(Bbox::new(
            CANONICAL_MIN_X,
            CANONICAL_MIN_Y,
            CANONICAL_MAX_X,
            CANONICAL_MAX_Y,
        ))
        .set_feature_count(FEATURES);
    w.finish().unwrap()
}

fn build_layer_bytes(style_ref: &str, source_hash: mars_types::ContentHash) -> Bytes {
    let class_assignment: Vec<(u64, u16)> = (0..FEATURES).map(|i| (i, 0)).collect();
    let mut w = ArtifactWriter::new(ArtifactKind::Layer);
    w.add_class_assignment(&class_assignment)
        .add_style_refs(&[style_ref.to_owned()])
        .set_bbox(Bbox::new(
            CANONICAL_MIN_X,
            CANONICAL_MIN_Y,
            CANONICAL_MAX_X,
            CANONICAL_MAX_Y,
        ))
        .set_feature_count(FEATURES)
        .set_source_ref(SourceRef {
            collection: COLLECTION.to_owned(),
            band: BAND.to_owned(),
            cell_x: 0,
            cell_y: 0,
            content_hash: source_hash,
        });
    w.finish().unwrap()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

async fn build_runtime(opts: BuildOpts) -> (Runtime, RenderPlan) {
    let store = Arc::new(InMemoryStore::new());
    let cache = Arc::new(InMemoryCache::new());

    let cell = Cell {
        band: ScaleBand::new(BAND),
        x: 0,
        y: 0,
    };

    let source_bytes = build_source_bytes(opts.geom);
    let source_hash = compute_content_hash(&source_bytes);
    let source_key_v = source_key(COLLECTION, &cell, &hex(&source_hash.0));
    store.put(&source_key_v, source_bytes.clone()).await.unwrap();

    let layer_ids: Vec<LayerId> = (0..opts.layers).map(|i| LayerId::new(format!("layer_{i}"))).collect();
    let style_refs: Vec<String> = (0..opts.layers).map(|i| format!("style_{i}")).collect();

    let mut layer_artifacts: Vec<ArtifactEntry> = Vec::with_capacity(opts.layers);
    let mut layer_index = hashbrown::HashMap::new();
    for (lid, sref) in layer_ids.iter().zip(style_refs.iter()) {
        let bytes = build_layer_bytes(sref, source_hash);
        let h = compute_content_hash(&bytes);
        let key = layer_key(lid, &cell, &hex(&h.0));
        store.put(&key, bytes.clone()).await.unwrap();
        let entry = ArtifactEntry {
            key,
            hash: h,
            size_bytes: bytes.len() as u64,
        };
        layer_index.insert(
            LayerCellKey {
                layer: lid.clone(),
                band: ScaleBand::new(BAND),
                x: 0,
                y: 0,
            },
            LayerCellState::Present(entry.clone()),
        );
        layer_artifacts.push(entry);
    }

    let source_artifact = ArtifactEntry {
        key: source_key_v,
        hash: source_hash,
        size_bytes: source_bytes.len() as u64,
    };
    let mut source_index = hashbrown::HashMap::new();
    source_index.insert(
        SourceCellKey {
            collection: Arc::<str>::from(COLLECTION),
            band: ScaleBand::new(BAND),
            x: 0,
            y: 0,
        },
        source_artifact.clone(),
    );

    let manifest = Manifest::new(1, "bench", vec![source_artifact], layer_artifacts, None, Vec::new());

    let mut stylesheet = Stylesheet::default();
    for sref in &style_refs {
        stylesheet.geometry.insert(
            sref.clone(),
            Arc::new(Style {
                fill: Some(Colour {
                    r: 200,
                    g: 100,
                    b: 50,
                    a: 255,
                }),
                ..Default::default()
            }),
        );
    }

    let canonical_crs = CrsCode::new("EPSG:25832");
    let state = RuntimeState {
        canonical_crs: canonical_crs.clone(),
        bands: vec![BandConfig {
            name: ScaleBand::new(BAND),
            max_denom: u32::MAX,
            origin: (CANONICAL_MIN_X, CANONICAL_MIN_Y),
            cell_size: CANONICAL_MAX_X - CANONICAL_MIN_X,
        }],
        layer_order: layer_ids.clone(),
        stylesheet,
        manifest,
        layer_index,
        source_index,
    };

    let deps = Deps {
        store,
        cache,
        renderer: Arc::new(NoopRenderer),
        encoder: Arc::new(NoopEncoder),
        metrics: mars_observability::Metrics::new().unwrap(),
        fonts: std::sync::Arc::new(mars_runtime::Fonts::with_default()),
    };
    let runtime = Runtime::from_state(Arc::new(state), deps);

    // keep the request bbox well inside the band's cell so the densified
    // reproject roundtrip (request -> canonical, with edge bulge) cannot
    // cross the cell boundary into an unmapped neighbour.
    let canonical_bbox = Bbox::new(
        CANONICAL_MIN_X + 2_000.0,
        CANONICAL_MIN_Y + 2_000.0,
        CANONICAL_MAX_X - 2_000.0,
        CANONICAL_MAX_Y - 2_000.0,
    );

    let (plan_bbox, plan_crs) = match opts.reproject_to {
        None => (canonical_bbox, canonical_crs.clone()),
        Some(target) => {
            // run a one-shot transform to get a bbox in the target CRS that
            // covers the same canonical region — guarantees the plan picks the
            // cell and exercises the reproject path.
            let target_crs = CrsCode::new(target);
            let to_target = mars_proj::Transformer::new(&canonical_crs, &target_crs).unwrap();
            let bbox_target = to_target.transform_bbox(canonical_bbox).unwrap();
            (bbox_target, target_crs)
        }
    };

    let plan = RenderPlan {
        layers: layer_ids,
        bbox: plan_bbox,
        width: 256,
        height: 256,
        crs: plan_crs,
        format: ImageFormat::Png,
    };

    (runtime, plan)
}

fn run_render_bench(c: &mut Criterion, group_name: &str, label: &str, opts: BuildOpts) {
    let rt = Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (runtime, plan) = rt.block_on(build_runtime(opts));

    // warm the per-thread proj cache before measuring so the first iter's
    // cold transformer build doesn't skew the warm-state numbers.
    if opts.reproject_to.is_some() {
        rt.block_on(runtime.render(&plan)).unwrap();
    }

    let mut group = c.benchmark_group(group_name);
    group.throughput(Throughput::Elements(FEATURES * opts.layers as u64));
    group.bench_with_input(
        BenchmarkId::from_parameter(label),
        &(runtime, plan),
        |b, (runtime, plan)| {
            b.iter(|| {
                let bytes = rt.block_on(runtime.render(plan)).unwrap();
                black_box(bytes.len())
            });
        },
    );
    group.finish();
}

fn bench_canonical(c: &mut Criterion) {
    run_render_bench(
        c,
        "runtime_render",
        "canonical/polygon_256",
        BuildOpts {
            geom: GeomShape::Polygon,
            layers: 1,
            reproject_to: None,
        },
    );
    run_render_bench(
        c,
        "runtime_render",
        "canonical/linestring_256",
        BuildOpts {
            geom: GeomShape::LineString,
            layers: 1,
            reproject_to: None,
        },
    );
}

fn bench_reproject(c: &mut Criterion) {
    run_render_bench(
        c,
        "runtime_render",
        "reproject_25832_to_3857/polygon_256",
        BuildOpts {
            geom: GeomShape::Polygon,
            layers: 1,
            reproject_to: Some("EPSG:3857"),
        },
    );
    run_render_bench(
        c,
        "runtime_render",
        "reproject_25832_to_4326/polygon_256",
        BuildOpts {
            geom: GeomShape::Polygon,
            layers: 1,
            reproject_to: Some("EPSG:4326"),
        },
    );
}

fn bench_multi_layer(c: &mut Criterion) {
    run_render_bench(
        c,
        "runtime_render",
        "canonical/polygon_256_x_3_layers",
        BuildOpts {
            geom: GeomShape::Polygon,
            layers: 3,
            reproject_to: None,
        },
    );
}

criterion_group!(benches, bench_canonical, bench_reproject, bench_multi_layer);
criterion_main!(benches);

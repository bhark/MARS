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
use std::time::Instant;

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

/// Bench layout knobs.
///
/// `cells_x` / `cells_y` shard a fixed canonical extent into a uniform grid;
/// the band's `cell_size` shrinks with the grid so plan resolution still
/// picks every cell. `features_per_cell` arranges features as a grid inside
/// each cell, so payload density and feature size scale with `cell_size`.
#[derive(Clone, Copy)]
struct BuildOpts {
    geom: GeomShape,
    layers: usize,
    cells_x: u32,
    cells_y: u32,
    features_per_cell: u64,
    line_len: usize,
    ring_len: usize,
    reproject_to: Option<&'static str>,
}

impl BuildOpts {
    fn single_cell(geom: GeomShape, layers: usize) -> Self {
        Self {
            geom,
            layers,
            cells_x: 1,
            cells_y: 1,
            features_per_cell: 256,
            line_len: 32,
            ring_len: 32,
            reproject_to: None,
        }
    }

    fn with_reproject(mut self, target: &'static str) -> Self {
        self.reproject_to = Some(target);
        self
    }

    fn with_grid(mut self, cells_x: u32, cells_y: u32, features_per_cell: u64) -> Self {
        self.cells_x = cells_x;
        self.cells_y = cells_y;
        self.features_per_cell = features_per_cell;
        self
    }

    fn with_vertex_count(mut self, line_len: usize, ring_len: usize) -> Self {
        self.line_len = line_len;
        self.ring_len = ring_len;
        self
    }

    fn total_features(&self) -> u64 {
        self.features_per_cell * self.layers as u64 * u64::from(self.cells_x) * u64::from(self.cells_y)
    }
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

/// Geometric description of one cell in the synthetic grid: integer cell
/// indices for the `SourceRef` and the f64 origin/size for placing features
/// inside the cell.
#[derive(Clone, Copy)]
struct CellGeom {
    cx: i64,
    cy: i64,
    origin_x: f64,
    origin_y: f64,
    size: f64,
}

impl CellGeom {
    fn bbox(&self) -> Bbox {
        Bbox::new(
            self.origin_x,
            self.origin_y,
            self.origin_x + self.size,
            self.origin_y + self.size,
        )
    }
}

/// Square-ish grid columns, biased so `cols * rows >= count`.
fn cell_grid_cols(count: u64) -> u64 {
    (count as f64).sqrt().ceil() as u64
}

fn make_polygon(id: u64, cell: &CellGeom, count: u64, ring_len: usize) -> FeatureGeom {
    let cols = cell_grid_cols(count).max(1);
    let stride = cell.size / cols as f64;
    let col = id % cols;
    let row = id / cols;
    let cx = cell.origin_x + (col as f64 + 0.5) * stride;
    let cy = cell.origin_y + (row as f64 + 0.5) * stride;
    let radius = stride * 0.3;

    let mut ring = Vec::with_capacity(ring_len + 1);
    for i in 0..ring_len {
        let theta = (i as f64) * std::f64::consts::TAU / (ring_len as f64);
        ring.push((cx + theta.cos() * radius, cy + theta.sin() * radius));
    }
    ring.push(ring[0]);
    let bbox = [
        (cx - radius) as f32,
        (cy - radius) as f32,
        (cx + radius) as f32,
        (cy + radius) as f32,
    ];
    FeatureGeom {
        id,
        bbox,
        geom: GeomKind::Polygon(vec![ring]),
    }
}

fn make_linestring(id: u64, cell: &CellGeom, count: u64, line_len: usize) -> FeatureGeom {
    let cols = cell_grid_cols(count).max(1);
    let stride = cell.size / cols as f64;
    let col = id % cols;
    let row = id / cols;
    let cx = cell.origin_x + (col as f64 + 0.1) * stride;
    let cy = cell.origin_y + (row as f64 + 0.5) * stride;
    let span = stride * 0.8;
    let dx_step = if line_len > 1 {
        span / (line_len - 1) as f64
    } else {
        0.0
    };
    let amp = stride * 0.05;

    let mut verts = Vec::with_capacity(line_len);
    for i in 0..line_len {
        let t = i as f64;
        verts.push((cx + t * dx_step, cy + (t * 0.4).sin() * amp));
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

fn build_source_bytes(opts: &BuildOpts, cell: &CellGeom) -> Bytes {
    let count = opts.features_per_cell;
    let features: Vec<FeatureGeom> = match opts.geom {
        GeomShape::Polygon => (0..count)
            .map(|id| make_polygon(id, cell, count, opts.ring_len))
            .collect(),
        GeomShape::LineString => (0..count)
            .map(|id| make_linestring(id, cell, count, opts.line_len))
            .collect(),
    };
    let mut w = ArtifactWriter::new(ArtifactKind::Source);
    w.add_geometry_payload(features)
        .set_bbox(cell.bbox())
        .set_feature_count(count);
    w.finish().unwrap()
}

fn build_layer_bytes(
    opts: &BuildOpts,
    style_ref: &str,
    source_hash: mars_types::ContentHash,
    cell: &CellGeom,
) -> Bytes {
    let count = opts.features_per_cell;
    let class_assignment: Vec<(u64, u16)> = (0..count).map(|i| (i, 0)).collect();
    let mut w = ArtifactWriter::new(ArtifactKind::Layer);
    w.add_class_assignment(&class_assignment)
        .add_style_refs(&[style_ref.to_owned()])
        .set_bbox(cell.bbox())
        .set_feature_count(count)
        .set_source_ref(SourceRef {
            collection: COLLECTION.to_owned(),
            band: BAND.to_owned(),
            cell_x: cell.cx,
            cell_y: cell.cy,
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

    // shrink the band's cell_size so the fixed canonical extent splits into
    // exactly cells_x * cells_y cells. for a 1x1 grid this collapses back to
    // the whole canonical extent.
    let canonical_w = CANONICAL_MAX_X - CANONICAL_MIN_X;
    let cells_n = opts.cells_x.max(opts.cells_y);
    assert!(cells_n >= 1);
    let cell_size = canonical_w / cells_n as f64;

    let layer_ids: Vec<LayerId> = (0..opts.layers).map(|i| LayerId::new(format!("layer_{i}"))).collect();
    let style_refs: Vec<String> = (0..opts.layers).map(|i| format!("style_{i}")).collect();

    let mut layer_artifacts: Vec<ArtifactEntry> = Vec::new();
    let mut source_artifacts: Vec<ArtifactEntry> = Vec::new();
    let mut layer_index = hashbrown::HashMap::new();
    let mut source_index = hashbrown::HashMap::new();

    for cy in 0..opts.cells_y as i64 {
        for cx in 0..opts.cells_x as i64 {
            let geom = CellGeom {
                cx,
                cy,
                origin_x: CANONICAL_MIN_X + (cx as f64) * cell_size,
                origin_y: CANONICAL_MIN_Y + (cy as f64) * cell_size,
                size: cell_size,
            };

            let source_bytes = build_source_bytes(&opts, &geom);
            let source_hash = compute_content_hash(&source_bytes);
            let cell = Cell {
                band: ScaleBand::new(BAND),
                x: cx,
                y: cy,
            };
            let source_key_v = source_key(COLLECTION, &cell, &hex(&source_hash.0));
            store.put(&source_key_v, source_bytes.clone()).await.unwrap();

            let source_entry = ArtifactEntry {
                key: source_key_v,
                hash: source_hash,
                size_bytes: source_bytes.len() as u64,
            };
            source_index.insert(
                SourceCellKey {
                    collection: Arc::<str>::from(COLLECTION),
                    band: ScaleBand::new(BAND),
                    x: cx,
                    y: cy,
                },
                source_entry.clone(),
            );
            source_artifacts.push(source_entry);

            for (lid, sref) in layer_ids.iter().zip(style_refs.iter()) {
                let bytes = build_layer_bytes(&opts, sref, source_hash, &geom);
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
                        x: cx,
                        y: cy,
                    },
                    LayerCellState::Present(entry.clone()),
                );
                layer_artifacts.push(entry);
            }
        }
    }

    let manifest = Manifest::new(1, "bench", source_artifacts, layer_artifacts, None, Vec::new());

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
            cell_size,
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

    // canonical bbox: 1x1 grids use an inset that keeps the reproject roundtrip
    // bulge inside the single cell. multi-cell grids span the full materialised
    // grid so plan::resolve picks every cell — reproject is not used in those
    // cases, so the bulge concern doesn't apply.
    let canonical_bbox = if opts.cells_x == 1 && opts.cells_y == 1 {
        Bbox::new(
            CANONICAL_MIN_X + 2_000.0,
            CANONICAL_MIN_Y + 2_000.0,
            CANONICAL_MAX_X - 2_000.0,
            CANONICAL_MAX_Y - 2_000.0,
        )
    } else {
        // inset by an epsilon to side-step the cells_in_bbox closed-interval
        // edge case: a bbox aligned exactly to a cell boundary picks an extra
        // row/column and silently inflates the cell count we're benching.
        let eps = cell_size * 1e-6;
        Bbox::new(
            CANONICAL_MIN_X + eps,
            CANONICAL_MIN_Y + eps,
            CANONICAL_MIN_X + cell_size * opts.cells_x as f64 - eps,
            CANONICAL_MIN_Y + cell_size * opts.cells_y as f64 - eps,
        )
    };

    let (plan_bbox, plan_crs) = match opts.reproject_to {
        None => (canonical_bbox, canonical_crs.clone()),
        Some(target) => {
            // one-shot transform to a target-CRS bbox covering the same canonical
            // region — guarantees the plan picks the cell and exercises reproject.
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

    // phase-share probe: one capture render outside the criterion measurement
    // loop. the global tracing layer is only armed for this single render so
    // the criterion samples below don't pay the layer's bookkeeping cost.
    let stats = phases::install();
    stats.activate();
    let t0 = Instant::now();
    let _ = rt.block_on(runtime.render(&plan)).unwrap();
    let total = t0.elapsed();
    stats.deactivate();
    phases::print_table(label, total, &stats.snapshot());
    stats.reset();

    let mut group = c.benchmark_group(group_name);
    group.throughput(Throughput::Elements(opts.total_features()));
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

/// Per-phase timing capture for the bench. A custom tracing layer accumulates
/// span busy time keyed by name; the bench harness arms it for one capture
/// render per case and prints the breakdown to stderr. Spans inside
/// spawn_blocking reach the layer because we install it as the **global**
/// default subscriber — `set_default` is thread-local and would not see the
/// blocking pool.
mod phases {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use tracing::{Subscriber, span};
    use tracing_subscriber::layer::{Context, Layer};
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::registry::{LookupSpan, Registry};

    #[derive(Default)]
    pub(super) struct Stats {
        active: AtomicBool,
        starts: Mutex<HashMap<span::Id, Instant>>,
        totals: Mutex<HashMap<&'static str, (u64, Duration)>>,
    }

    impl Stats {
        pub(super) fn activate(&self) {
            self.active.store(true, Ordering::Relaxed);
        }
        pub(super) fn deactivate(&self) {
            self.active.store(false, Ordering::Relaxed);
        }
        pub(super) fn reset(&self) {
            self.starts.lock().unwrap().clear();
            self.totals.lock().unwrap().clear();
        }
        pub(super) fn snapshot(&self) -> Vec<(&'static str, Duration)> {
            self.totals.lock().unwrap().iter().map(|(k, (_, d))| (*k, *d)).collect()
        }
    }

    struct PhaseLayer(Arc<Stats>);

    impl<S> Layer<S> for PhaseLayer
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_enter(&self, id: &span::Id, _ctx: Context<'_, S>) {
            if !self.0.active.load(Ordering::Relaxed) {
                return;
            }
            self.0.starts.lock().unwrap().insert(id.clone(), Instant::now());
        }
        fn on_exit(&self, id: &span::Id, ctx: Context<'_, S>) {
            if !self.0.active.load(Ordering::Relaxed) {
                return;
            }
            let Some(start) = self.0.starts.lock().unwrap().remove(id) else {
                return;
            };
            let elapsed = start.elapsed();
            let name = ctx.span(id).map(|s| s.metadata().name()).unwrap_or("?");
            let mut totals = self.0.totals.lock().unwrap();
            let entry = totals.entry(name).or_insert((0, Duration::ZERO));
            entry.0 += 1;
            entry.1 += elapsed;
        }
    }

    static STATS: OnceLock<Arc<Stats>> = OnceLock::new();

    pub(super) fn install() -> Arc<Stats> {
        STATS
            .get_or_init(|| {
                let stats = Arc::new(Stats::default());
                let subscriber = Registry::default().with(PhaseLayer(stats.clone()));
                let _ = tracing::subscriber::set_global_default(subscriber);
                stats
            })
            .clone()
    }

    pub(super) fn print_table(label: &str, total: Duration, snap: &[(&'static str, Duration)]) {
        // canonical order so the table reads top-down through the pipeline.
        let order = [
            "runtime.render",
            "fetch.layer_artifacts",
            "fetch.source_artifacts",
            "cpu.emit",
            "cpu.labels",
            "cpu.raster",
            "cpu.encode",
        ];
        eprintln!("== phase shares: {label} (wall {total:?}) ==");
        for name in order {
            if let Some((_, dur)) = snap.iter().find(|(n, _)| *n == name) {
                let share = if total.is_zero() {
                    0.0
                } else {
                    dur.as_secs_f64() / total.as_secs_f64() * 100.0
                };
                eprintln!("  {name:<28} {dur:>10.2?} ({share:>5.1}%)");
            }
        }
    }
}

fn bench_canonical(c: &mut Criterion) {
    run_render_bench(
        c,
        "runtime_render",
        "canonical/polygon_256",
        BuildOpts::single_cell(GeomShape::Polygon, 1),
    );
    run_render_bench(
        c,
        "runtime_render",
        "canonical/linestring_256",
        BuildOpts::single_cell(GeomShape::LineString, 1),
    );
}

fn bench_reproject(c: &mut Criterion) {
    run_render_bench(
        c,
        "runtime_render",
        "reproject_25832_to_3857/polygon_256",
        BuildOpts::single_cell(GeomShape::Polygon, 1).with_reproject("EPSG:3857"),
    );
    run_render_bench(
        c,
        "runtime_render",
        "reproject_25832_to_4326/polygon_256",
        BuildOpts::single_cell(GeomShape::Polygon, 1).with_reproject("EPSG:4326"),
    );
}

fn bench_multi_layer(c: &mut Criterion) {
    run_render_bench(
        c,
        "runtime_render",
        "canonical/polygon_256_x_3_layers",
        BuildOpts::single_cell(GeomShape::Polygon, 3),
    );
}

/// Phase 1 coverage: workloads that stress axes the existing cases miss.
/// Composite_100_layers exercises the cpu.emit O(N) scaling that motivates
/// parallel emit. Many-cells exercises plan resolution + per-cell fetch fan
/// out. Tiny-payload many-cells reproduces the per-call-overhead-dominated
/// regression class where features are cheap but emit is called often.
fn bench_phase1_coverage(c: &mut Criterion) {
    run_render_bench(
        c,
        "runtime_render",
        "canonical/composite_100_layers_linestring",
        BuildOpts::single_cell(GeomShape::LineString, 100),
    );
    run_render_bench(
        c,
        "runtime_render",
        "canonical/single_layer_10x10_cells_polygon",
        BuildOpts::single_cell(GeomShape::Polygon, 1).with_grid(10, 10, 256),
    );
    run_render_bench(
        c,
        "runtime_render",
        "canonical/single_layer_10x10_cells_tiny_linestring",
        BuildOpts::single_cell(GeomShape::LineString, 1)
            .with_grid(10, 10, 4)
            .with_vertex_count(4, 4),
    );
}

criterion_group!(
    benches,
    bench_canonical,
    bench_reproject,
    bench_multi_layer,
    bench_phase1_coverage
);
criterion_main!(benches);

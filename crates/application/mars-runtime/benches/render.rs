//! end-to-end Runtime::render against in-memory store/cache and a noop
//! renderer/encoder. exercises decode + cull + project on a polygon-heavy cell.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use bytes::Bytes;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
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
const LAYER: &str = "parcels";
const BAND: &str = "hi";
const STYLE: &str = "fill";
const FEATURES: u64 = 256;
const RING_LEN: usize = 32;

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
    let cx = 100.0 + (id as f64) * 2.0;
    let cy = 100.0 + ((id % 16) as f64) * 2.0;
    let mut ring = Vec::with_capacity(RING_LEN + 1);
    for i in 0..RING_LEN {
        let theta = (i as f64) * std::f64::consts::TAU / (RING_LEN as f64);
        ring.push((cx + theta.cos(), cy + theta.sin()));
    }
    ring.push(ring[0]);
    let bbox = [
        (cx - 1.0) as f32,
        (cy - 1.0) as f32,
        (cx + 1.0) as f32,
        (cy + 1.0) as f32,
    ];
    FeatureGeom {
        id,
        bbox,
        geom: GeomKind::Polygon(vec![ring]),
    }
}

fn build_source_bytes() -> Bytes {
    let features: Vec<FeatureGeom> = (0..FEATURES).map(make_polygon).collect();
    let mut w = ArtifactWriter::new(ArtifactKind::Source);
    w.add_geometry_payload(features)
        .set_bbox(Bbox::new(0.0, 0.0, 1024.0, 1024.0))
        .set_feature_count(FEATURES);
    w.finish().unwrap()
}

fn build_layer_bytes(source_hash: mars_types::ContentHash) -> Bytes {
    let class_assignment: Vec<(u64, u16)> = (0..FEATURES).map(|i| (i, 0)).collect();
    let mut w = ArtifactWriter::new(ArtifactKind::Layer);
    w.add_class_assignment(&class_assignment)
        .add_style_refs(&[STYLE.to_owned()])
        .set_bbox(Bbox::new(0.0, 0.0, 1024.0, 1024.0))
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

async fn build_runtime() -> (Runtime, RenderPlan) {
    let store = Arc::new(InMemoryStore::new());
    let cache = Arc::new(InMemoryCache::new());

    let cell = Cell {
        band: ScaleBand::new(BAND),
        x: 0,
        y: 0,
    };
    let source_bytes = build_source_bytes();
    let source_hash = compute_content_hash(&source_bytes);
    let layer_bytes = build_layer_bytes(source_hash);
    let layer_hash = compute_content_hash(&layer_bytes);

    let source_key_v = source_key(COLLECTION, &cell, &hex(&source_hash.0));
    let layer_key_v = layer_key(&LayerId::new(LAYER), &cell, &hex(&layer_hash.0));
    store.put(&source_key_v, source_bytes.clone()).await.unwrap();
    store.put(&layer_key_v, layer_bytes.clone()).await.unwrap();

    let manifest = Manifest::new(
        1,
        "bench",
        vec![ArtifactEntry {
            key: source_key_v,
            hash: source_hash,
            size_bytes: source_bytes.len() as u64,
        }],
        vec![ArtifactEntry {
            key: layer_key_v,
            hash: layer_hash,
            size_bytes: layer_bytes.len() as u64,
        }],
        None,
        Vec::new(),
    );

    let mut layer_index = hashbrown::HashMap::new();
    layer_index.insert(
        LayerCellKey {
            layer: LayerId::new(LAYER),
            band: ScaleBand::new(BAND),
            x: 0,
            y: 0,
        },
        LayerCellState::Present(manifest.layer_artifacts[0].clone()),
    );
    let mut source_index = hashbrown::HashMap::new();
    source_index.insert(
        SourceCellKey {
            collection: Arc::<str>::from(COLLECTION),
            band: ScaleBand::new(BAND),
            x: 0,
            y: 0,
        },
        manifest.source_artifacts[0].clone(),
    );

    let mut stylesheet = Stylesheet::default();
    stylesheet.geometry.insert(
        STYLE.to_owned(),
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

    let canonical_crs = CrsCode::new("EPSG:25832");
    let state = RuntimeState {
        canonical_crs: canonical_crs.clone(),
        bands: vec![BandConfig {
            name: ScaleBand::new(BAND),
            max_denom: u32::MAX,
            origin: (0.0, 0.0),
            cell_size: 1024.0,
        }],
        layer_order: vec![LayerId::new(LAYER)],
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
    };
    let runtime = Runtime::from_state(Arc::new(state), deps);

    let plan = RenderPlan {
        layers: vec![LayerId::new(LAYER)],
        bbox: Bbox::new(0.0, 0.0, 1023.0, 1023.0),
        width: 256,
        height: 256,
        crs: canonical_crs,
        format: ImageFormat::Png,
    };

    (runtime, plan)
}

fn bench_render(c: &mut Criterion) {
    let rt = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (runtime, plan) = rt.block_on(build_runtime());

    c.bench_function("runtime_render_canonical_crs", |b| {
        b.iter(|| {
            let bytes = rt.block_on(runtime.render(&plan)).unwrap();
            black_box(bytes.len())
        });
    });
}

criterion_group!(benches, bench_render);
criterion_main!(benches);

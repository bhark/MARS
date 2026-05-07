//! determinism guard for the rayon-driven emit phase. SPEC §10 requires
//! z-order to follow the plan's layer/cell vector; rayon work-stealing must
//! not perturb the per-render `DrawOp` sequence regardless of worker count
//! or chunk size.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support {
    pub(crate) mod mock_renderer;
}

use std::sync::Arc;

use bytes::Bytes;
use mars_artifact::{ArtifactKind, ArtifactWriter, FeatureGeom, GeomKind, SourceRef, compute_content_hash};
use mars_config::ParallelEmit;
use mars_grid::BandConfig;
use mars_render_port::{DrawOp, Renderer};
use mars_runtime::{
    DecodedGeometryCache, Deps, RenderPlan, Runtime, RuntimeState,
    key::{layer_key, source_key},
    state::{LayerCellKey, LayerCellState, SourceCellKey},
};
use mars_store::ObjectStore;
use mars_store::mem::{InMemoryCache, InMemoryStore};
use mars_style::{Colour, Style, Stylesheet};
use mars_types::{ArtifactEntry, Bbox, Cell, ContentHash, CrsCode, ImageFormat, LayerId, Manifest, ScaleBand};

use crate::support::mock_renderer::{CannedEncoder, MockRenderer};

const COLLECTION: &str = "parcels_src";
const BAND: &str = "hi";
const STYLE: &str = "fill";

fn make_polygon(id: u64, offset: f64) -> FeatureGeom {
    let x0 = offset;
    let x1 = offset + 5.0;
    FeatureGeom {
        id,
        bbox: [x0 as f32, 0.0, x1 as f32, 5.0],
        geom: GeomKind::Polygon(vec![vec![(x0, 0.0), (x1, 0.0), (x1, 5.0), (x0, 5.0), (x0, 0.0)]]),
    }
}

fn build_source_bytes(cx: i64, cy: i64) -> Bytes {
    let base_x = cx as f64 * 100.0;
    let base_y = cy as f64 * 100.0;
    let features: Vec<FeatureGeom> = (0..8).map(|i| make_polygon(i, base_x + 10.0 * i as f64)).collect();
    let mut w = ArtifactWriter::new(ArtifactKind::Source);
    w.add_geometry_payload(features)
        .set_bbox(Bbox::new(base_x, base_y, base_x + 100.0, base_y + 10.0))
        .set_feature_count(8);
    w.finish().unwrap()
}

fn build_layer_bytes(source_hash: ContentHash, cx: i64, cy: i64) -> Bytes {
    let class_assignment: Vec<(u64, u16)> = (0..8).map(|i| (i, 0)).collect();
    let style_refs = vec![STYLE.to_owned()];
    let mut w = ArtifactWriter::new(ArtifactKind::Layer);
    let base_x = cx as f64 * 100.0;
    let base_y = cy as f64 * 100.0;
    w.add_class_assignment(&class_assignment)
        .add_style_refs(&style_refs)
        .set_bbox(Bbox::new(base_x, base_y, base_x + 100.0, base_y + 10.0))
        .set_feature_count(8)
        .set_source_ref(SourceRef {
            collection: COLLECTION.to_owned(),
            band: BAND.to_owned(),
            cell_x: cx,
            cell_y: cy,
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

struct Fixture {
    store: Arc<InMemoryStore>,
    state: Arc<RuntimeState>,
    plan: RenderPlan,
}

async fn build_fixture(layers: usize, cells_x: i64, cells_y: i64) -> Fixture {
    let store = Arc::new(InMemoryStore::new());

    let cell_size = 100.0;
    let layer_ids: Vec<LayerId> = (0..layers).map(|i| LayerId::new(format!("layer_{i}"))).collect();

    let mut layer_artifacts: Vec<ArtifactEntry> = Vec::new();
    let mut source_artifacts: Vec<ArtifactEntry> = Vec::new();
    let mut layer_index = hashbrown::HashMap::new();
    let mut source_index = hashbrown::HashMap::new();

    for cy in 0..cells_y {
        for cx in 0..cells_x {
            let source_bytes = build_source_bytes(cx, cy);
            let source_hash = compute_content_hash(&source_bytes);
            let cell = Cell {
                band: ScaleBand::new(BAND),
                x: cx,
                y: cy,
            };
            let s_key = source_key(COLLECTION, &cell, &hex(&source_hash.0));
            store.put(&s_key, source_bytes.clone()).await.unwrap();
            let s_entry = ArtifactEntry {
                key: s_key,
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
                s_entry.clone(),
            );
            source_artifacts.push(s_entry);

            for lid in &layer_ids {
                let bytes = build_layer_bytes(source_hash, cx, cy);
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

    let manifest = Manifest::new(1, "test", source_artifacts, layer_artifacts, None, Vec::new());

    let mut stylesheet = Stylesheet::default();
    stylesheet.geometry.insert(
        STYLE.to_owned(),
        Arc::new(Style {
            fill: Some(Colour {
                r: 10,
                g: 20,
                b: 30,
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
            cell_size,
        }],
        layer_order: layer_ids.clone(),
        stylesheet,
        manifest,
        layer_index,
        source_index,
    };

    // inset by an epsilon so the request bbox does not land exactly on cell
    // boundaries (cells_in_bbox uses inclusive ..= edges).
    let plan = RenderPlan {
        layers: layer_ids,
        bbox: Bbox::new(
            0.001,
            0.001,
            cell_size * cells_x as f64 - 0.001,
            cell_size * cells_y as f64 - 0.001,
        ),
        width: 256,
        height: 256,
        crs: canonical_crs,
        format: ImageFormat::Png,
    };

    Fixture {
        store,
        state: Arc::new(state),
        plan,
    }
}

fn build_runtime(fx: &Fixture, parallel_emit: ParallelEmit, mock: Arc<MockRenderer>) -> Runtime {
    let renderer: Arc<dyn Renderer> = mock;
    let deps = Deps {
        store: fx.store.clone(),
        cache: Arc::new(InMemoryCache::new()),
        renderer,
        encoder: Arc::new(CannedEncoder),
        metrics: mars_observability::Metrics::new().unwrap(),
        fonts: Arc::new(mars_runtime::Fonts::with_default()),
    };
    Runtime::with_full_config(
        deps,
        128_000_000,
        Arc::new(DecodedGeometryCache::new(64 * 1024 * 1024)),
        parallel_emit,
        Some(fx.state.clone()),
    )
}

fn render_under_pool(fx: &Fixture, parallel_emit: ParallelEmit, threads: usize) -> Vec<DrawOp> {
    let mock = Arc::new(MockRenderer::default());
    let runtime = build_runtime(fx, parallel_emit, mock.clone());
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
    pool.install(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _ = runtime.render(&fx.plan).await.unwrap();
        });
    });
    let mut recorded = mock.ops.lock().unwrap();
    assert_eq!(recorded.len(), 1, "renderer called exactly once per render");
    recorded.remove(0)
}

fn ops_signature(ops: &[DrawOp]) -> Vec<String> {
    ops.iter().map(|op| format!("{op:?}")).collect()
}

#[test]
fn parallel_emit_preserves_op_order_across_thread_counts() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let fx = rt.block_on(build_fixture(8, 1, 1));

    let cfg = ParallelEmit {
        enabled: true,
        chunk_size: 1,
    };
    let baseline = render_under_pool(&fx, cfg, 1);
    let parallel_8 = render_under_pool(&fx, cfg, 8);
    let parallel_4 = render_under_pool(&fx, cfg, 4);

    assert_eq!(ops_signature(&baseline), ops_signature(&parallel_8));
    assert_eq!(ops_signature(&baseline), ops_signature(&parallel_4));
}

#[test]
fn parallel_emit_matches_serial_path() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let fx = rt.block_on(build_fixture(4, 1, 1));

    let serial = render_under_pool(
        &fx,
        ParallelEmit {
            enabled: false,
            chunk_size: 8,
        },
        1,
    );
    let parallel = render_under_pool(
        &fx,
        ParallelEmit {
            enabled: true,
            chunk_size: 1,
        },
        8,
    );

    assert_eq!(ops_signature(&serial), ops_signature(&parallel));
}

#[test]
fn parallel_emit_preserves_order_with_multi_cell_plan() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // 3x3 cells × 3 layers = 27 emit tasks: enough work to stress rayon
    // scheduling without inflating fixture build time.
    let fx = rt.block_on(build_fixture(3, 3, 3));

    let baseline = render_under_pool(
        &fx,
        ParallelEmit {
            enabled: false,
            chunk_size: 8,
        },
        1,
    );
    let parallel = render_under_pool(
        &fx,
        ParallelEmit {
            enabled: true,
            chunk_size: 2,
        },
        8,
    );

    assert_eq!(ops_signature(&baseline), ops_signature(&parallel));
}

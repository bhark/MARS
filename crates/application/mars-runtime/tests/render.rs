//! integration tests for `mars_runtime::Runtime`. uses `FsStore`+`FsCache` from
//! the `mars-store-fs` adapter (dev-dep) plus pre-baked artifacts written via
//! `mars_artifact::ArtifactWriter`. the renderer is a recording mock.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support {
    pub(crate) mod mock_renderer;
}

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use mars_artifact::{ArtifactKind, ArtifactWriter, FeatureGeom, GeomKind, SourceRef, compute_content_hash};
use mars_grid::BandConfig;
use mars_render_port::{DrawOp, Renderer};
use mars_runtime::{
    Deps, RenderPlan, Runtime, RuntimeError, RuntimeState,
    key::{layer_key, source_key},
};
use mars_store::{LocalCache, ObjectStore, StoreError};
use mars_store_fs::{FsCache, FsStore};
use mars_style::{Colour, Style, Stylesheet};
use mars_types::{ArtifactEntry, Bbox, Cell, CrsCode, ImageFormat, LayerId, Manifest, ScaleBand};
use tempfile::TempDir;
use tokio::sync::Notify;

use crate::support::mock_renderer::{CANNED_BYTES, MockRenderer};

const COLLECTION: &str = "parcels_src";
const LAYER: &str = "parcels";
const BAND: &str = "hi";
const CELL_X: i64 = 0;
const CELL_Y: i64 = 0;

const STYLE_RED: &str = "red_fill";
const STYLE_BLUE: &str = "blue_fill";

/// a single 10x10 polygon offset by (offset, 0) in world coords.
fn make_polygon(id: u64, offset: f64) -> FeatureGeom {
    let x0 = offset;
    let y0 = 0.0;
    let x1 = offset + 10.0;
    let y1 = 10.0;
    FeatureGeom {
        id,
        bbox: [x0 as f32, y0 as f32, x1 as f32, y1 as f32],
        geom: GeomKind::Polygon(vec![vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)]]),
    }
}

fn build_source_bytes(offset: f64) -> Bytes {
    let features: Vec<FeatureGeom> = (0..5).map(|i| make_polygon(i, offset + 20.0 * i as f64)).collect();
    let bbox = Bbox::new(offset, 0.0, offset + 100.0, 10.0);
    let mut w = ArtifactWriter::new(ArtifactKind::Source);
    w.add_geometry_payload(&features)
        .unwrap()
        .set_bbox(bbox)
        .set_feature_count(features.len() as u64);
    w.finish().unwrap()
}

fn build_layer_bytes(source_hash: mars_types::ContentHash) -> Bytes {
    // 5 features, alternating between two classes.
    let class_assignment: Vec<(u64, u16)> = (0..5).map(|i| (i as u64, (i % 2) as u16)).collect();
    // class 0 -> STYLE_RED, class 1 -> STYLE_BLUE
    let style_refs = vec![STYLE_RED.to_owned(), STYLE_BLUE.to_owned()];

    let mut w = ArtifactWriter::new(ArtifactKind::Layer);
    w.add_class_assignment(&class_assignment)
        .add_style_refs(&style_refs)
        .set_bbox(Bbox::new(0.0, 0.0, 100.0, 10.0))
        .set_feature_count(5)
        .set_source_ref(SourceRef {
            collection: COLLECTION.to_owned(),
            band: BAND.to_owned(),
            cell_x: CELL_X,
            cell_y: CELL_Y,
            content_hash: source_hash,
        });
    w.finish().unwrap()
}

fn stylesheet() -> Stylesheet {
    let mut ss = Stylesheet::default();
    ss.geometry.insert(
        STYLE_RED.to_owned(),
        Style {
            fill: Some(Colour {
                r: 255,
                g: 0,
                b: 0,
                a: 255,
            }),
            ..Default::default()
        },
    );
    ss.geometry.insert(
        STYLE_BLUE.to_owned(),
        Style {
            fill: Some(Colour {
                r: 0,
                g: 0,
                b: 255,
                a: 255,
            }),
            ..Default::default()
        },
    );
    ss
}

struct Fixture {
    _tmp: TempDir,
    runtime: Runtime,
    mock: Arc<MockRenderer>,
    store: Arc<FsStore>,
    canonical_crs: CrsCode,
}

async fn build_fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let store_root = tmp.path().join("store");
    let cache_root = tmp.path().join("cache");
    std::fs::create_dir_all(&store_root).unwrap();
    std::fs::create_dir_all(&cache_root).unwrap();

    let store = Arc::new(FsStore::new(store_root).unwrap());
    let cache = FsCache::new(cache_root, u64::MAX).unwrap();

    let manifest = write_manifest(&store, 1, 0.0).await;

    let canonical_crs = CrsCode::new("EPSG:25832");
    let state = state_from_manifest(canonical_crs.clone(), manifest);

    let mock = Arc::new(MockRenderer::default());
    let renderer: Arc<dyn Renderer> = mock.clone();
    let deps = Deps {
        store: store.clone(),
        cache: Arc::new(cache),
        renderer,
    };
    let runtime = Runtime::from_state(Arc::new(state), deps);

    Fixture {
        _tmp: tmp,
        runtime,
        mock,
        store,
        canonical_crs,
    }
}

async fn write_manifest(store: &FsStore, version: u64, offset: f64) -> Manifest {
    let cell = Cell {
        band: ScaleBand::new(BAND),
        x: CELL_X,
        y: CELL_Y,
    };

    let source_bytes = build_source_bytes(offset);
    let source_hash = compute_content_hash(&source_bytes);
    let layer_bytes = build_layer_bytes(source_hash);
    let layer_hash = compute_content_hash(&layer_bytes);

    let source_key_v = source_key(COLLECTION, &cell, &hex(&source_hash.0));
    let layer_key_v = layer_key(&LayerId::new(LAYER), &cell, &hex(&layer_hash.0));

    store.put(&source_key_v, source_bytes.clone()).await.unwrap();
    store.put(&layer_key_v, layer_bytes.clone()).await.unwrap();

    Manifest {
        version,
        service: "test".into(),
        source_artifacts: vec![ArtifactEntry {
            key: source_key_v,
            hash: source_hash,
            size_bytes: source_bytes.len() as u64,
        }],
        layer_artifacts: vec![ArtifactEntry {
            key: layer_key_v,
            hash: layer_hash,
            size_bytes: layer_bytes.len() as u64,
        }],
        style_artifact: None,
    }
}

fn state_from_manifest(canonical_crs: CrsCode, manifest: Manifest) -> RuntimeState {
    let bands = vec![BandConfig {
        name: ScaleBand::new(BAND),
        max_denom: u32::MAX,
        origin: (0.0, 0.0),
        cell_size: 1024.0,
    }];

    let mut layer_index = std::collections::HashMap::new();
    let mut source_index = std::collections::HashMap::new();
    layer_index.insert(
        (LayerId::new(LAYER), ScaleBand::new(BAND), (CELL_X, CELL_Y)),
        manifest.layer_artifacts[0].clone(),
    );
    source_index.insert(
        (COLLECTION.to_owned(), ScaleBand::new(BAND), (CELL_X, CELL_Y)),
        manifest.source_artifacts[0].clone(),
    );

    RuntimeState {
        canonical_crs,
        bands,
        layer_order: vec![LayerId::new(LAYER)],
        stylesheet: stylesheet(),
        manifest,
        layer_index,
        source_index,
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[derive(Clone)]
struct BlockingCache {
    inner: FsCache,
    should_block: Arc<AtomicBool>,
    reached: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl LocalCache for BlockingCache {
    async fn get_or_fetch(
        &self,
        key: &mars_types::ArtifactKey,
        expected: mars_types::ContentHash,
        origin: &dyn ObjectStore,
    ) -> Result<Bytes, StoreError> {
        if self.should_block.swap(false, Ordering::SeqCst) {
            self.reached.notify_waiters();
            self.release.notified().await;
        }
        self.inner.get_or_fetch(key, expected, origin).await
    }

    fn mark_evictable(&self, key: &mars_types::ArtifactKey) {
        self.inner.mark_evictable(key);
    }
}

fn plan_for(fixture: &Fixture) -> RenderPlan {
    RenderPlan {
        layers: vec![LayerId::new(LAYER)],
        bbox: Bbox::new(0.0, 0.0, 200.0, 10.0),
        width: 256,
        height: 64,
        crs: fixture.canonical_crs.clone(),
        format: ImageFormat::Png,
    }
}

fn ops_debug(ops: &[DrawOp]) -> Vec<String> {
    ops.iter().map(|op| format!("{op:?}")).collect()
}

#[tokio::test]
async fn renders_expected_paths() {
    let fx = build_fixture().await;
    let bytes = fx.runtime.render(&plan_for(&fx)).await.unwrap();
    assert_eq!(bytes, CANNED_BYTES);

    let recorded = fx.mock.ops.lock().unwrap();
    assert_eq!(recorded.len(), 1, "renderer called exactly once");
    let ops = &recorded[0];
    let path_count = ops.iter().filter(|o| matches!(o, DrawOp::Path { .. })).count();
    assert_eq!(path_count, 5, "one DrawOp::Path per feature");
    assert!(
        ops.iter().all(|o| matches!(o, DrawOp::Path { .. })),
        "no DrawOp::Label expected in phase 0"
    );
}

#[tokio::test]
async fn empty_runtime_returns_not_ready() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(FsStore::new(tmp.path().join("store")).unwrap());
    let cache = Arc::new(FsCache::new(tmp.path().join("cache"), u64::MAX).unwrap());
    let mock = Arc::new(MockRenderer::default());
    let runtime = Runtime::empty(Deps {
        store,
        cache,
        renderer: mock,
    });
    assert!(!runtime.is_ready());

    let fixture = build_fixture().await;
    match runtime.render(&plan_for(&fixture)).await {
        Err(RuntimeError::NotReady) => {}
        other => panic!("expected NotReady, got {other:?}"),
    }
}

#[tokio::test]
async fn swap_state_makes_runtime_ready_and_renderable() {
    let fx = build_fixture().await;
    let runtime = Runtime::empty(Deps {
        store: fx.store.clone(),
        cache: Arc::new(FsCache::new(fx._tmp.path().join("cache2"), u64::MAX).unwrap()),
        renderer: fx.mock.clone(),
    });
    assert!(!runtime.is_ready());
    runtime.swap_state(fx.runtime.current_state().unwrap());
    assert!(runtime.is_ready());

    let bytes = runtime.render(&plan_for(&fx)).await.unwrap();
    assert_eq!(bytes, CANNED_BYTES);
}

#[tokio::test]
async fn swap_state_changes_rendered_ops_without_rebuilding_runtime() {
    let fx = build_fixture().await;
    let plan = plan_for(&fx);
    fx.runtime.render(&plan).await.unwrap();
    let first = ops_debug(&fx.mock.ops.lock().unwrap()[0]);

    let second_manifest = write_manifest(&fx.store, 2, 100.0).await;
    fx.runtime
        .swap_state(Arc::new(state_from_manifest(fx.canonical_crs.clone(), second_manifest)));
    fx.runtime.render(&plan).await.unwrap();
    let recorded = fx.mock.ops.lock().unwrap();
    assert_ne!(first, ops_debug(&recorded[1]));
}

#[tokio::test]
async fn render_pins_state_across_swap() {
    let tmp = TempDir::new().unwrap();
    let store_root = tmp.path().join("store");
    let cache_root = tmp.path().join("cache");
    std::fs::create_dir_all(&store_root).unwrap();
    std::fs::create_dir_all(&cache_root).unwrap();

    let store = Arc::new(FsStore::new(store_root).unwrap());
    let cache = FsCache::new(cache_root, u64::MAX).unwrap();
    let first_manifest = write_manifest(&store, 1, 0.0).await;
    let second_manifest = write_manifest(&store, 2, 100.0).await;
    let canonical_crs = CrsCode::new("EPSG:25832");
    let first_state = state_from_manifest(canonical_crs.clone(), first_manifest.clone());
    let second_state = state_from_manifest(canonical_crs.clone(), second_manifest);

    let reached = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let blocking_cache = BlockingCache {
        inner: cache,
        should_block: Arc::new(AtomicBool::new(true)),
        reached: reached.clone(),
        release: release.clone(),
    };
    let mock = Arc::new(MockRenderer::default());
    let runtime = Arc::new(Runtime::from_state(
        Arc::new(first_state),
        Deps {
            store: store.clone(),
            cache: Arc::new(blocking_cache),
            renderer: mock.clone(),
        },
    ));
    let plan = RenderPlan {
        layers: vec![LayerId::new(LAYER)],
        bbox: Bbox::new(0.0, 0.0, 200.0, 10.0),
        width: 256,
        height: 64,
        crs: canonical_crs.clone(),
        format: ImageFormat::Png,
    };
    let expected_renderer = Arc::new(MockRenderer::default());
    let expected_runtime = Runtime::from_state(
        Arc::new(state_from_manifest(canonical_crs.clone(), first_manifest)),
        Deps {
            store: store.clone(),
            cache: Arc::new(FsCache::new(tmp.path().join("expected-cache"), u64::MAX).unwrap()),
            renderer: expected_renderer.clone(),
        },
    );
    expected_runtime.render(&plan).await.unwrap();
    let expected_old = ops_debug(&expected_renderer.ops.lock().unwrap()[0]);

    let render_task = {
        let runtime = runtime.clone();
        let plan = plan.clone();
        tokio::spawn(async move { runtime.render(&plan).await })
    };
    reached.notified().await;
    runtime.swap_state(Arc::new(second_state));
    release.notify_waiters();
    render_task.await.unwrap().unwrap();

    let recorded = mock.ops.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(ops_debug(&recorded[0]), expected_old);
}

#[tokio::test]
async fn rejects_non_canonical_crs() {
    let fx = build_fixture().await;
    let mut plan = plan_for(&fx);
    plan.crs = CrsCode::new("EPSG:3857");
    match fx.runtime.render(&plan).await {
        Err(RuntimeError::CrsNotCanonical { requested }) => {
            assert_eq!(requested, "EPSG:3857");
        }
        other => panic!("expected CrsNotCanonical, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_manifest_entry_errors() {
    let fx = build_fixture().await;
    let mut plan = plan_for(&fx);
    plan.layers = vec![LayerId::new("does-not-exist")];
    match fx.runtime.render(&plan).await {
        Err(RuntimeError::ManifestEntryMissing { layer, .. }) => {
            assert_eq!(layer, "does-not-exist");
        }
        other => panic!("expected ManifestEntryMissing, got {other:?}"),
    }
}

#[tokio::test]
async fn deterministic_repeat() {
    let fx = build_fixture().await;
    let _ = fx.runtime.render(&plan_for(&fx)).await.unwrap();
    let _ = fx.runtime.render(&plan_for(&fx)).await.unwrap();
    let recorded = fx.mock.ops.lock().unwrap();
    assert_eq!(recorded.len(), 2);
    let format = |op: &DrawOp| format!("{op:?}");
    let a: Vec<String> = recorded[0].iter().map(format).collect();
    let b: Vec<String> = recorded[1].iter().map(format).collect();
    assert_eq!(a, b, "draw op sequence must be deterministic");
}

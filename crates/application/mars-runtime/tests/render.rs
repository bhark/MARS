//! integration tests for `mars_runtime::Runtime`. uses `FsStore`+`FsCache` from
//! the `mars-store-fs` adapter (dev-dep) plus pre-baked artifacts written via
//! `mars_artifact::ArtifactWriter`. the renderer is a recording mock.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support {
    pub(crate) mod mock_renderer;
}

use std::sync::Arc;

use bytes::Bytes;
use mars_artifact::{
    ArtifactKind, ArtifactWriter, FeatureGeom, GeomKind, SourceRef, compute_content_hash,
};
use mars_grid::BandConfig;
use mars_render_port::{DrawOp, Renderer};
use mars_runtime::{
    Deps, RenderPlan, Runtime, RuntimeError, RuntimeState,
    key::{layer_key, source_key},
};
use mars_store::ObjectStore;
use mars_store_fs::{FsCache, FsStore};
use mars_style::{Colour, Style, Stylesheet};
use mars_types::{
    ArtifactEntry, Bbox, Cell, CrsCode, ImageFormat, LayerId, Manifest, ScaleBand,
};
use tempfile::TempDir;

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
        geom: GeomKind::Polygon(vec![vec![
            (x0, y0),
            (x1, y0),
            (x1, y1),
            (x0, y1),
            (x0, y0),
        ]]),
    }
}

fn build_source_bytes() -> Bytes {
    let features: Vec<FeatureGeom> = (0..5).map(|i| make_polygon(i, 20.0 * i as f64)).collect();
    let bbox = Bbox::new(0.0, 0.0, 100.0, 10.0);
    let mut w = ArtifactWriter::new(ArtifactKind::Source);
    w.add_geometry_payload(&features).unwrap()
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
            fill: Some(Colour { r: 255, g: 0, b: 0, a: 255 }),
            ..Default::default()
        },
    );
    ss.geometry.insert(
        STYLE_BLUE.to_owned(),
        Style {
            fill: Some(Colour { r: 0, g: 0, b: 255, a: 255 }),
            ..Default::default()
        },
    );
    ss
}

struct Fixture {
    _tmp: TempDir,
    runtime: Runtime,
    mock: Arc<MockRenderer>,
    canonical_crs: CrsCode,
}

async fn build_fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let store_root = tmp.path().join("store");
    let cache_root = tmp.path().join("cache");
    std::fs::create_dir_all(&store_root).unwrap();
    std::fs::create_dir_all(&cache_root).unwrap();

    let store = FsStore::new(store_root).unwrap();
    let cache = FsCache::new(cache_root).unwrap();

    let cell = Cell {
        band: ScaleBand::new(BAND),
        x: CELL_X,
        y: CELL_Y,
    };

    // bake source + compute hash, then bake layer that references it.
    let source_bytes = build_source_bytes();
    let source_hash = compute_content_hash(&source_bytes);
    let layer_bytes = build_layer_bytes(source_hash);
    let layer_hash = compute_content_hash(&layer_bytes);

    let source_key_v = source_key(COLLECTION, &cell, &hex(&source_hash.0));
    let layer_key_v = layer_key(&LayerId::new(LAYER), &cell, &hex(&layer_hash.0));

    store.put(&source_key_v, source_bytes.clone()).await.unwrap();
    store.put(&layer_key_v, layer_bytes.clone()).await.unwrap();

    let manifest = Manifest {
        version: 1,
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
    };

    let canonical_crs = CrsCode::new("EPSG:25832");
    let bands = vec![BandConfig {
        name: ScaleBand::new(BAND),
        max_denom: u32::MAX,
        origin: (0.0, 0.0),
        cell_size: 1024.0,
    }];

    // build state directly (bypassing Config) — simpler for tests.
    let mut layer_index = std::collections::HashMap::new();
    let mut source_index = std::collections::HashMap::new();
    layer_index.insert(
        (
            LayerId::new(LAYER),
            ScaleBand::new(BAND),
            (CELL_X, CELL_Y),
        ),
        manifest.layer_artifacts[0].clone(),
    );
    source_index.insert(
        (COLLECTION.to_owned(), ScaleBand::new(BAND), (CELL_X, CELL_Y)),
        manifest.source_artifacts[0].clone(),
    );

    let state = RuntimeState {
        canonical_crs: canonical_crs.clone(),
        bands,
        layer_order: vec![LayerId::new(LAYER)],
        stylesheet: stylesheet(),
        manifest,
        layer_index,
        source_index,
    };

    let mock = Arc::new(MockRenderer::default());
    let renderer: Arc<dyn Renderer> = mock.clone();
    let deps = Deps {
        store: Arc::new(store),
        cache: Arc::new(cache),
        renderer,
    };
    let runtime = Runtime::from_state(Arc::new(state), deps);

    Fixture {
        _tmp: tmp,
        runtime,
        mock,
        canonical_crs,
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
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

#[tokio::test]
async fn renders_expected_paths() {
    let fx = build_fixture().await;
    let bytes = fx.runtime.render(&plan_for(&fx)).await.unwrap();
    assert_eq!(bytes, CANNED_BYTES);

    let recorded = fx.mock.ops.lock().unwrap();
    assert_eq!(recorded.len(), 1, "renderer called exactly once");
    let ops = &recorded[0];
    let path_count = ops
        .iter()
        .filter(|o| matches!(o, DrawOp::Path { .. }))
        .count();
    assert_eq!(path_count, 5, "one DrawOp::Path per feature");
    assert!(
        ops.iter().all(|o| matches!(o, DrawOp::Path { .. })),
        "no DrawOp::Label expected in phase 0"
    );
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

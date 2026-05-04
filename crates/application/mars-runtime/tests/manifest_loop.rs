//! tests for `run_manifest_reload_loop`: version monotonicity, reject reason
//! tracking, and reload acceptance.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream::{self, BoxStream};
use mars_artifact::compute_content_hash;
use mars_config::{
    ArtifactCache, ArtifactStore, Artifacts, Band, Cells, Config, Scales, ServiceMeta, Source,
};
use mars_render_port::{Canvas, DrawOp, Renderer};
use mars_runtime::{Deps, Runtime, run_manifest_reload_loop};
use mars_store::mem::{InMemoryCache, InMemoryStore};
use mars_store::{ManifestWatch, ObjectStore, StoreError};
use mars_style::Stylesheet;
use mars_types::{ArtifactEntry, ArtifactKey, CrsCode, ImageFormat, Manifest};

struct VecWatch {
    items: std::sync::Mutex<Option<Vec<Result<Manifest, StoreError>>>>,
}

#[async_trait]
impl ManifestWatch for VecWatch {
    async fn watch(&self) -> Result<BoxStream<'static, Result<Manifest, StoreError>>, StoreError> {
        let items = self.items.lock().unwrap().take().unwrap_or_default();
        Ok(Box::pin(stream::iter(items)))
    }
}

struct NopRenderer;
impl Renderer for NopRenderer {
    fn render(
        &self,
        _canvas: Canvas,
        _ops: &[DrawOp],
        _format: ImageFormat,
    ) -> Result<Vec<u8>, mars_render_port::RenderError> {
        Ok(Vec::new())
    }
}

fn minimal_config() -> Arc<Config> {
    let mut size_per_band = BTreeMap::new();
    size_per_band.insert("hi".into(), "4096m".into());
    Arc::new(Config {
        service: ServiceMeta {
            name: "t".into(),
            ..Default::default()
        },
        source: Source {
            kind: "memory".into(),
            dsn: "memory://".into(),
            native_crs: CrsCode::new("EPSG:25832"),
            change_feed: None,
        },
        artifacts: Artifacts {
            store: ArtifactStore {
                kind: "fs".into(),
                endpoint: None,
                bucket: None,
                prefix: None,
                path: Some("/tmp".into()),
            },
            cache: ArtifactCache {
                path: "/tmp".into(),
                max_size: "1GiB".into(),
                eviction: "lru".into(),
            },
        },
        scales: Scales {
            bands: vec![Band {
                name: "hi".into(),
                max_denom: 25_000,
            }],
        },
        cells: Cells {
            grid: "regular".into(),
            origin: [0.0, 0.0],
            size_per_band,
            extent: None,
        },
        interfaces: Default::default(),
        tile_matrix_sets: Default::default(),
        reprojection: Default::default(),
        styles: Default::default(),
        layers: vec![],
        observability: Default::default(),
    })
}

fn manifest(version: u64) -> Manifest {
    Manifest {
        version,
        service: "t".into(),
        source_artifacts: vec![],
        layer_artifacts: vec![],
        style_artifact: None,
    }
}

fn manifest_with_bad_key(version: u64) -> Manifest {
    // a layer-shaped key in source_artifacts is rejected by RuntimeState.
    let key = ArtifactKey::new("lyr/whatever/hi/0_0/v1/abcd.mars");
    let body = Bytes::from_static(b"x");
    let hash = compute_content_hash(&body);
    Manifest {
        version,
        service: "t".into(),
        source_artifacts: vec![ArtifactEntry {
            key,
            hash,
            size_bytes: 1,
        }],
        layer_artifacts: vec![],
        style_artifact: None,
    }
}

fn build_runtime() -> Arc<Runtime> {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemoryStore::new());
    let cache = Arc::new(InMemoryCache::new());
    let renderer = Arc::new(NopRenderer);
    Arc::new(Runtime::empty(Deps {
        store,
        cache,
        renderer,
    }))
}

#[tokio::test]
async fn loop_accepts_increasing_versions_and_clears_reject() {
    let runtime = build_runtime();
    let watch = Arc::new(VecWatch {
        items: std::sync::Mutex::new(Some(vec![Ok(manifest(1)), Ok(manifest(2))])),
    });
    run_manifest_reload_loop(runtime.clone(), watch, minimal_config(), Stylesheet::default())
        .await
        .unwrap();
    let state = runtime.current_state().expect("state present");
    assert_eq!(state.manifest.version, 2);
    assert!(runtime.last_reject_reason().is_none(), "no reject reason after success");
}

#[tokio::test]
async fn loop_rejects_older_version_and_records_reason() {
    let runtime = build_runtime();
    let watch = Arc::new(VecWatch {
        items: std::sync::Mutex::new(Some(vec![Ok(manifest(5)), Ok(manifest(3))])),
    });
    run_manifest_reload_loop(runtime.clone(), watch, minimal_config(), Stylesheet::default())
        .await
        .unwrap();
    let state = runtime.current_state().expect("state present");
    assert_eq!(state.manifest.version, 5, "older v3 must be rejected");
    let reason = runtime.last_reject_reason().expect("reject reason recorded");
    assert!(reason.contains("3"), "reason mentions rejected version: {reason}");
    assert!(reason.contains("5"), "reason mentions current version: {reason}");
}

#[tokio::test]
async fn loop_records_reason_when_state_build_fails() {
    let runtime = build_runtime();
    let watch = Arc::new(VecWatch {
        items: std::sync::Mutex::new(Some(vec![Ok(manifest_with_bad_key(7))])),
    });
    run_manifest_reload_loop(runtime.clone(), watch, minimal_config(), Stylesheet::default())
        .await
        .unwrap();
    assert!(runtime.current_state().is_none(), "no state on rejection");
    let reason = runtime.last_reject_reason().expect("reject reason recorded");
    assert!(reason.contains("v7"), "reason mentions version: {reason}");
}

#[tokio::test]
async fn loop_records_reason_for_invalid_snapshot() {
    let runtime = build_runtime();
    let watch = Arc::new(VecWatch {
        items: std::sync::Mutex::new(Some(vec![Err(StoreError::Backend("garbled".into()))])),
    });
    run_manifest_reload_loop(runtime.clone(), watch, minimal_config(), Stylesheet::default())
        .await
        .unwrap();
    let reason = runtime.last_reject_reason().expect("reject reason recorded");
    assert!(reason.contains("invalid snapshot"), "reason: {reason}");
}

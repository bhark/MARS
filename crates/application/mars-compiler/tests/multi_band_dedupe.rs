//! Regression: a layer with two source bindings that resolve to the same
//! `(layer_id, binding_id)` (different `band:` only) must not produce duplicate
//! class sidecars in the published manifest. SPEC §7.3 — bands are routing
//! rules, not substrate axes.
//!
//! Without `build_bootstrap_plan` deduping `LayerPlan` per `(layer_id,
//! binding_id)`, `emit_layer_sidecars` runs twice per page and the runtime
//! rejects the manifest at swap with `IndexError::DuplicateSidecar`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::stream;
use mars_compiler::plan::build_bootstrap_plan;
use mars_compiler::testing::FullScanCompileSession;
use mars_compiler::{Deps, run_snapshot_from_plan};
use mars_config::{
    Artifacts, Band, Cells, Class, ClassStyle, Compiler, Config, Interfaces, Layer, Observability, Render, Scales,
    ServiceMeta, Source, SourceBinding,
};
use mars_observability::Metrics;
use mars_source::{
    AttrValue, ChangeFeed, ChangeSubscription, CompileSession, LeaderLock, LeaderLockGuard, RowBytes,
    Source as PortSource, SourceBinding as PortBinding, SourceError, SourceRowKey,
};
use mars_store::mem::{InMemoryPublisher, InMemoryStore};
use mars_types::{Bbox, CrsCode, LayerId};

const WORKING_SET: u64 = 4 * 1024 * 1024 * 1024;
const PLAN_BUDGET: u64 = 8 * 1024 * 1024 * 1024;
const IN_FLIGHT_BUDGET: u64 = 4 * 1024 * 1024 * 1024;

fn point_wkb(x: f64, y: f64) -> Bytes {
    let mut v = Vec::with_capacity(21);
    v.push(1);
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&x.to_le_bytes());
    v.extend_from_slice(&y.to_le_bytes());
    Bytes::from(v)
}

fn row(id: u64, x: f64, y: f64) -> RowBytes {
    RowBytes {
        feature_id: id,
        geometry: point_wkb(x, y),
        attributes: vec![("vejkategori".into(), AttrValue::String("Stor vej".into()))],
        row_key: SourceRowKey::ZERO,
    }
}

#[derive(Default)]
struct FakeSource {
    rows: Mutex<HashMap<u64, RowBytes>>,
}

impl FakeSource {
    fn with_rows(rows: Vec<RowBytes>) -> Self {
        let map: HashMap<u64, RowBytes> = rows.into_iter().map(|r| (r.feature_id, r)).collect();
        Self { rows: Mutex::new(map) }
    }
}

#[async_trait]
impl PortSource for FakeSource {
    async fn fetch_full_table_streaming<'a>(
        &'a self,
        _binding: &'a PortBinding,
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        let mut owned: Vec<RowBytes> = self.rows.lock().unwrap().values().cloned().collect();
        owned.sort_by_key(|r| r.feature_id);
        Ok(Box::pin(stream::iter(owned.into_iter().map(Ok))))
    }

    async fn fetch_by_feature_ids<'a>(
        &'a self,
        _binding: &'a PortBinding,
        ids: &'a [i64],
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        let lock = self.rows.lock().unwrap();
        let owned: Vec<RowBytes> = ids.iter().filter_map(|i| lock.get(&(*i as u64)).cloned()).collect();
        Ok(Box::pin(stream::iter(owned.into_iter().map(Ok))))
    }

    async fn stream_feature_ids<'a>(
        &'a self,
        _binding: &'a PortBinding,
    ) -> Result<BoxStream<'a, Result<i64, SourceError>>, SourceError> {
        let lock = self.rows.lock().unwrap();
        let mut ids: Vec<i64> = lock.keys().map(|id| *id as i64).collect();
        ids.sort();
        Ok(Box::pin(stream::iter(ids.into_iter().map(Ok))))
    }

    async fn open_compile_session<'a>(
        &'a self,
        binding: &'a PortBinding,
    ) -> Result<Box<dyn CompileSession + 'a>, SourceError> {
        Ok(FullScanCompileSession::boxed(self, binding))
    }
}

#[derive(Default)]
struct NopFeed;
#[async_trait]
impl ChangeFeed for NopFeed {
    async fn subscribe(&self) -> Result<Box<dyn ChangeSubscription>, SourceError> {
        Err(SourceError::NotImplemented { what: "test feed" })
    }
}
#[derive(Default)]
struct NopLock;
#[async_trait]
impl LeaderLock for NopLock {
    async fn try_acquire(&self, _key: i64) -> Result<Option<Box<dyn LeaderLockGuard>>, SourceError> {
        Err(SourceError::NotImplemented { what: "test lock" })
    }
}

fn build_two_band_config() -> Config {
    let mut size_per_band = std::collections::BTreeMap::new();
    size_per_band.insert("hi".into(), "1024m".into());
    size_per_band.insert("mid".into(), "4096m".into());
    let make_source = |band: &str| SourceBinding {
        scale: None,
        band: Some(band.into()),
        max_denom: None,
        from: "vejmidte".into(),
        geometry_column: "geom".into(),
        id_column: Some("id".into()),
        attributes: vec!["vejkategori".into()],
        levels: None,
        page_size_target_bytes: None,
        reconcile_every_cycles: None,
        sidecar_size_warn_bytes: None,
        simplifier: None,
    };
    Config {
        service: ServiceMeta {
            name: "test".into(),
            ..Default::default()
        },
        source: Source {
            kind: "memory".into(),
            dsn: "memory://".into(),
            native_crs: CrsCode::new("EPSG:25832"),
            change_feed: None,
            pool: Default::default(),
        },
        artifacts: Artifacts {
            store: mars_config::ArtifactStore {
                kind: "fs".into(),
                endpoint: None,
                bucket: None,
                prefix: None,
                path: Some("/tmp".into()),
                allow_http: false,
            },
            cache: mars_config::ArtifactCache {
                path: "/tmp".into(),
                max_size: "1GiB".into(),
                eviction: "lru".into(),
                trust_path_hash: false,
            },
        },
        scales: Scales {
            bands: vec![
                Band {
                    name: "hi".into(),
                    max_denom: 25_000,
                },
                Band {
                    name: "mid".into(),
                    max_denom: 250_000,
                },
            ],
        },
        cells: Cells {
            grid: "regular".into(),
            origin: [0.0, 0.0],
            size_per_band,
            extent: Some(Bbox::new(0.0, 0.0, 1_000.0, 1_000.0)),
        },
        interfaces: Interfaces::default(),
        tile_matrix_sets: Default::default(),
        reprojection: Default::default(),
        styles: Default::default(),
        layers: vec![Layer {
            name: LayerId::new("Vejmidte"),
            title: String::new(),
            abstract_: String::new(),
            kind: "line".into(),
            scale: None,
            group: None,
            enable_get_feature_info: false,
            bbox: None,
            sources: vec![make_source("hi"), make_source("mid")],
            classes: vec![Class {
                name: "stor".into(),
                title: String::new(),
                when: Some("vejkategori = 'Stor vej'".into()),
                style: ClassStyle::Inline(Default::default()),
            }],
            label: None,
            label_survival: mars_config::LabelSurvival::Independent,
        }],
        observability: Observability::default(),
        render: Render::default(),
        compiler: Compiler::default(),
    }
}

#[tokio::test]
async fn multi_band_same_binding_emits_one_class_sidecar_per_page() {
    let cfg = build_two_band_config();
    let plan = build_bootstrap_plan(&cfg).expect("plan");
    assert_eq!(plan.bindings.len(), 1, "two band sources collapse to one binding");
    assert_eq!(plan.layers.len(), 1, "two band sources collapse to one layer plan");

    let rows: Vec<RowBytes> = (0..40u64)
        .map(|i| row(i, f64::from(i as u32) * 5.0, f64::from(i as u32) * 7.0))
        .collect();
    let source = Arc::new(FakeSource::with_rows(rows));
    let store = Arc::new(InMemoryStore::new());
    let manifest = Arc::new(InMemoryPublisher::new());
    let deps = Deps {
        source,
        change_feed: Arc::new(NopFeed),
        leader_lock: Arc::new(NopLock),
        store,
        manifest,
        metrics: Metrics::new().unwrap(),
    };

    let m = run_snapshot_from_plan(
        &deps,
        &plan,
        "test".into(),
        1,
        WORKING_SET,
        PLAN_BUDGET,
        IN_FLIGHT_BUDGET,
        1,
        &std::env::temp_dir(),
        256,
    )
    .await
    .expect("snapshot");

    assert!(!m.class_sidecars.is_empty(), "fixture must produce sidecars");
    let mut seen: HashSet<(String, _)> = HashSet::new();
    for sc in &m.class_sidecars {
        let key = (sc.layer_id.as_str().to_string(), sc.page_key.clone());
        assert!(
            seen.insert(key.clone()),
            "duplicate class sidecar for {:?} — dedupe regression",
            key
        );
    }
}

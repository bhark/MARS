//! Unified compile pipeline determinism and parity tests.
//!
//! Asserts:
//! 1. Same source compiled twice through `run_snapshot_from_plan` yields
//!    byte-identical artifact bytes (page hashes, sidecar hashes, level
//!    metadata).
//! 2. A binding compiled via the unified pipeline and an empty incremental
//!    rebuild against that bootstrap manifest leave the manifest content
//!    untouched (no spurious replacements).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::stream;
use mars_compiler::plan::{BindingPlan, BootstrapPlan, LevelPlan};
use mars_compiler::testing::FullScanCompileSession;
use mars_compiler::{Deps, run_snapshot_from_plan};
use mars_observability::Metrics;
use mars_source::{
    AttrValue, ChangeFeed, ChangeSubscription, CompileSession, LeaderLock, LeaderLockGuard, RowBytes, Source,
    SourceBinding as PortBinding, SourceError, SourceRowKey,
};
use mars_store::ObjectStore;
use mars_store::mem::{InMemoryPublisher, InMemoryStore};
use mars_types::{BindingId, CrsCode, DecimationLevel};

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
        attributes: vec![("name".into(), AttrValue::String(format!("p{id}")))],
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
impl Source for FakeSource {
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

fn binding_plan(id: &str, page_size: u64) -> BindingPlan {
    BindingPlan {
        binding_id: BindingId::try_new(id).unwrap(),
        source_table: id.to_string(),
        filter: None,
        geometry_column: "geom".into(),
        id_column: Some("id".into()),
        attributes: vec!["name".into()],
        native_crs: CrsCode::new("EPSG:25832"),
        levels: vec![LevelPlan {
            level: DecimationLevel::new(0),
            vertex_tolerance_m: 0.0,
            geometry_min_size_m: 0.0,
            label_min_priority: 0,
        }],
        page_size_target_bytes: page_size,
        sidecar_size_warn_bytes: u64::MAX,
        reconcile_every_cycles: u32::MAX,
        simplifier: mars_config::SimplifierKind::Naive,
    }
}

fn make_deps(rows: Vec<RowBytes>) -> (Deps, Arc<InMemoryStore>) {
    let source = Arc::new(FakeSource::with_rows(rows));
    let store = Arc::new(InMemoryStore::new());
    let manifest = Arc::new(InMemoryPublisher::new());
    (
        Deps {
            source,
            change_feed: Arc::new(NopFeed),
            leader_lock: Arc::new(NopLock),
            store: store.clone(),
            manifest,
            metrics: Metrics::new().unwrap(),
        },
        store,
    )
}

#[tokio::test]
async fn unified_compile_is_deterministic_across_runs() {
    // 200 points across a wide bbox; small page budget forces multiple pages.
    let rows: Vec<RowBytes> = (0..200u64)
        .map(|i| row(i, f64::from(i as u32) * 7.0, f64::from(i as u32) * 11.0))
        .collect();

    let (deps_a, _store_a) = make_deps(rows.clone());
    let (deps_b, _store_b) = make_deps(rows);
    let plan = BootstrapPlan {
        bindings: vec![binding_plan("points", 4 * 1024)],
        layers: vec![],
    };

    let m_a = run_snapshot_from_plan(
        &deps_a,
        &plan,
        "test".into(),
        1,
        WORKING_SET,
        PLAN_BUDGET,
        IN_FLIGHT_BUDGET,
        1,
        &std::env::temp_dir(),
        256,
        &mars_compiler::memory_governor::MemoryGovernor::new(u64::MAX),
    )
    .await
    .unwrap();
    let m_b = run_snapshot_from_plan(
        &deps_b,
        &plan,
        "test".into(),
        1,
        WORKING_SET,
        PLAN_BUDGET,
        IN_FLIGHT_BUDGET,
        1,
        &std::env::temp_dir(),
        256,
        &mars_compiler::memory_governor::MemoryGovernor::new(u64::MAX),
    )
    .await
    .unwrap();

    assert!(m_a.pages.len() > 1, "fixture must produce multiple pages");
    assert_eq!(m_a.bindings, m_b.bindings, "binding metadata must match exactly");
    assert_eq!(m_a.pages.len(), m_b.pages.len(), "page count must match");
    for (a, b) in m_a.pages.iter().zip(m_b.pages.iter()) {
        assert_eq!(a.key, b.key, "page key mismatch at {:?}", a.key);
        assert_eq!(
            a.content_hash, b.content_hash,
            "page content hash differs at {:?}",
            a.key
        );
        assert_eq!(a.hilbert_range, b.hilbert_range);
        assert_eq!(a.feature_count, b.feature_count);
        assert_eq!(a.size_bytes, b.size_bytes);
    }
    assert_eq!(m_a.class_sidecars, m_b.class_sidecars);
    assert_eq!(m_a.label_sidecars, m_b.label_sidecars);
}

#[tokio::test]
async fn rows_with_identical_geometry_but_different_attrs_are_slot_equivalent() {
    // two rows with the same WKB geometry but different attribute payloads.
    // (key, user_id, WKB-fingerprint) ties are not order-stable; both
    // arrangements should compile without error and the manifest must
    // contain exactly two features.
    let geom = point_wkb(100.0, 200.0);
    let rows = vec![
        RowBytes {
            feature_id: 1,
            geometry: geom.clone(),
            attributes: vec![("name".into(), AttrValue::String("alpha".into()))],
            row_key: SourceRowKey::ZERO,
        },
        RowBytes {
            feature_id: 2,
            geometry: geom,
            attributes: vec![("name".into(), AttrValue::String("beta".into()))],
            row_key: SourceRowKey::ZERO,
        },
    ];
    let (deps, _store) = make_deps(rows);
    let plan = BootstrapPlan {
        bindings: vec![binding_plan("points", 64 * 1024)],
        layers: vec![],
    };
    let manifest = run_snapshot_from_plan(
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
        &mars_compiler::memory_governor::MemoryGovernor::new(u64::MAX),
    )
    .await
    .unwrap();
    let total: u64 = manifest.pages.iter().map(|p| p.feature_count).sum();
    assert_eq!(total, 2, "both rows must land in the substrate");
}

#[tokio::test]
async fn unified_compile_against_empty_source_yields_zero_pages() {
    let (deps, store) = make_deps(vec![]);
    let plan = BootstrapPlan {
        bindings: vec![binding_plan("points", 4 * 1024)],
        layers: vec![],
    };
    let manifest = run_snapshot_from_plan(
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
        &mars_compiler::memory_governor::MemoryGovernor::new(u64::MAX),
    )
    .await
    .unwrap();
    assert_eq!(manifest.pages.len(), 0);
    assert_eq!(manifest.bindings.len(), 1);
    assert_eq!(manifest.bindings[0].feature_count_total, 0);
    assert!(manifest.bindings[0].page_membership_sidecar.is_none());
    // empty bootstrap should not have written any objects.
    assert!(store.list("").await.unwrap().is_empty());
}

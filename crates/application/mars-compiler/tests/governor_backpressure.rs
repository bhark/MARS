//! tier B: prove the compile cycle actually honours the memory governor.
//!
//! the in-module governor tests pin the primitives (acquire / try_acquire /
//! release-via-drop) but every existing cycle-level test passes `u64::MAX`
//! caps, leaving "the wiring is real" untested. this file runs a snapshot
//! through `run_snapshot_from_plan` with a tight cap and asserts the cycle
//! still completes (no deadlock) and peak_bytes stayed ≤ cap.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::Mutex as StdMutex;

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
use mars_store::mem::{InMemoryPublisher, InMemoryStore};
use mars_types::{BindingId, CrsCode, DecimationLevel};

const FEATURE_COUNT: u64 = 1_000;
// the snapshot run still needs working-set / in-flight headroom to bootstrap
// at all; this test specifically tightens the memory governor.
const TEST_WORKING_SET: u64 = 4 * 1024 * 1024 * 1024;
const TEST_PLAN_BUDGET: u64 = 8 * 1024 * 1024 * 1024;
const TEST_IN_FLIGHT_BUDGET: u64 = 4 * 1024 * 1024 * 1024;
// memory governor cap. small enough that a 1k-row bootstrap cannot complete
// in one in-flight batch and must release-and-reacquire mid-flight.
const MEM_CAP: u64 = 16 * 1024;

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
    rows: StdMutex<Vec<RowBytes>>,
}

impl FakeSource {
    fn with_rows(rows: Vec<RowBytes>) -> Self {
        Self {
            rows: StdMutex::new(rows),
        }
    }
}

#[async_trait]
impl Source for FakeSource {
    async fn stream_rows<'a>(
        &'a self,
        _binding: &'a PortBinding,
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        let mut owned = self.rows.lock().unwrap().clone();
        owned.sort_by_key(|r| r.feature_id);
        Ok(Box::pin(stream::iter(owned.into_iter().map(Ok))))
    }

    async fn stream_rows_by_id<'a>(
        &'a self,
        _binding: &'a PortBinding,
        ids: &'a [i64],
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        let lock = self.rows.lock().unwrap();
        let owned: Vec<RowBytes> = lock
            .iter()
            .filter(|r| ids.contains(&(r.feature_id as i64)))
            .cloned()
            .collect();
        Ok(Box::pin(stream::iter(owned.into_iter().map(Ok))))
    }

    async fn stream_feature_ids<'a>(
        &'a self,
        _binding: &'a PortBinding,
    ) -> Result<BoxStream<'a, Result<i64, SourceError>>, SourceError> {
        let lock = self.rows.lock().unwrap();
        let mut ids: Vec<i64> = lock.iter().map(|r| r.feature_id as i64).collect();
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
        Err(SourceError::NotImplemented {
            what: "test ChangeFeed",
        })
    }
}

#[derive(Default)]
struct NopLock;
#[async_trait]
impl LeaderLock for NopLock {
    async fn try_acquire(&self, _key: i64) -> Result<Option<Box<dyn LeaderLockGuard>>, SourceError> {
        Err(SourceError::NotImplemented {
            what: "test LeaderLock",
        })
    }
}

fn binding_plan(id: &str) -> BindingPlan {
    BindingPlan {
        binding_id: BindingId::try_new(id).unwrap(),
        source_id: mars_config::SourceId::new("default"),
        source_table: id.to_string(),
        filter: None,
        geometry_field: "geom".into(),
        id_field: Some("id".into()),
        attributes: vec!["name".into()],
        native_crs: CrsCode::new("EPSG:25832"),
        levels: vec![LevelPlan {
            level: DecimationLevel::new(0),
            vertex_tolerance_m: 0.0,
            geometry_min_size_m: 0.0,
            label_min_priority: 0,
        }],
        page_size_target_bytes: 1024,
        sidecar_size_warn_bytes: u64::MAX,
        reconcile_every_cycles: 24,
        simplifier: mars_config::SimplifierKind::Naive,
        missing_page_policy: mars_config::MissingPagePolicy::Truncate,
        dsn: None,
    }
}

fn make_deps(source: Arc<FakeSource>) -> Deps {
    let store = Arc::new(InMemoryStore::new());
    let manifest = Arc::new(InMemoryPublisher::new());
    let mut registry = mars_compiler::SourceRegistry::new();
    registry.insert(mars_config::SourceId::new("default"), source);
    Deps {
        sources: Arc::new(registry),
        change_feed: Arc::new(NopFeed),
        leader_lock: Arc::new(NopLock),
        store,
        manifest,
        metrics: Metrics::new().unwrap(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_completes_under_tight_memory_governor_cap() {
    let rows: Vec<RowBytes> = (0..FEATURE_COUNT)
        .map(|i| row(i, f64::from(i as u32), f64::from(i as u32)))
        .collect();
    let source = Arc::new(FakeSource::with_rows(rows));
    let deps = make_deps(source);

    let plan = BootstrapPlan {
        bindings: vec![binding_plan("points")],
        layers: vec![],
        raster_layers: vec![],
    };

    let memory = mars_compiler::memory_governor::MemoryGovernor::new(MEM_CAP);
    let disk = mars_compiler::disk_governor::DiskGovernor::new(u64::MAX);

    // 30-second wall clock ceiling: if the cycle deadlocks against the tight
    // cap we want a fast, deterministic failure rather than a CI timeout.
    let bootstrap = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run_snapshot_from_plan(
            &deps,
            &plan,
            "test".into(),
            1,
            TEST_WORKING_SET,
            TEST_PLAN_BUDGET,
            TEST_IN_FLIGHT_BUDGET,
            1,
            &std::env::temp_dir(),
            256,
            &memory,
            &disk,
        ),
    )
    .await
    .expect("snapshot must not deadlock under tight memory governor")
    .expect("snapshot must succeed");

    // every feature accounted for in the published manifest. proves the
    // governor's release-and-reacquire path actually completes the cycle,
    // not just survives the first batch.
    let total: u64 = bootstrap.pages.iter().map(|p| p.feature_count).sum();
    assert_eq!(
        total, FEATURE_COUNT,
        "post-bootstrap feature count = {total}, expected {FEATURE_COUNT}"
    );

    // peak in-flight stayed within cap throughout the cycle. if peak exceeds
    // cap the governor wiring is a no-op for the snapshot path.
    let peak = memory.peak_bytes();
    eprintln!("memory governor peak_bytes = {peak}, cap = {MEM_CAP}");
    assert!(peak <= MEM_CAP, "memory governor peak {peak} exceeded cap {MEM_CAP}");
    // peak == 0 would mean the snapshot path never called the governor at
    // all - "wiring is a no-op". with 1000 features the governor must have
    // been consulted at least once.
    assert!(
        peak > 0,
        "memory governor peak is zero; snapshot path never consulted the governor"
    );
}

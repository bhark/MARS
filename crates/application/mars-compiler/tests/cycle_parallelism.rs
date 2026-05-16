//! Cycle-rebuild bounded-parallelism regression tests.
//!
//! Two properties guarded here:
//! 1. concurrency_proof: when `binding_parallelism >= N`, all N bindings
//!    enter their per-binding rebuild concurrently. Asserted via a shared
//!    `tokio::sync::Barrier::new(N)` inside the source - under sequential
//!    execution the first binding's `stream_rows` would block forever and
//!    the test would hit its timeout.
//! 2. isolate_policy: one binding failing under `BindingFailurePolicy::
//!    Isolate` does not prevent the surviving bindings from contributing
//!    to the rebuild outcome. Exercised at `binding_parallelism = 1` and
//!    `= 4` to confirm the policy is concurrency-invariant.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::stream;
use mars_compiler::incremental::{BindingDirty, DirtyPages};
use mars_compiler::plan::{BindingPlan, BootstrapPlan, LevelPlan};
use mars_compiler::render::rebuild_pages;
use mars_compiler::sidecar::SidecarReader;
use mars_compiler::testing::FullScanCompileSession;
use mars_compiler::{Deps, run_snapshot_from_plan};
use mars_observability::Metrics;
use mars_source::{
    AttrValue, ChangeFeed, ChangeSubscription, CompileSession, LeaderLock, LeaderLockGuard, RowBytes, Source,
    SourceBinding as PortBinding, SourceError, SourceRowKey,
};
use mars_store::mem::{InMemoryPublisher, InMemoryStore};
use mars_types::{BindingId, CrsCode, DecimationLevel};

const TEST_WORKING_SET: u64 = 4 * 1024 * 1024 * 1024;
const TEST_PLAN_BUDGET: u64 = 8 * 1024 * 1024 * 1024;
const TEST_IN_FLIGHT_BUDGET: u64 = 4 * 1024 * 1024 * 1024;

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
struct PerBindingMode {
    rows: Vec<RowBytes>,
    fail: bool,
}

/// fake source keyed by binding collection id. supports per-binding
/// failure injection and an optional shared rebuild-time barrier.
#[derive(Default)]
struct ConfigurableFakeSource {
    bindings: StdMutex<HashMap<String, PerBindingMode>>,
    rebuild_barrier: StdMutex<Option<Arc<tokio::sync::Barrier>>>,
}

impl ConfigurableFakeSource {
    fn add_binding(&self, name: &str, rows: Vec<RowBytes>) {
        self.bindings
            .lock()
            .unwrap()
            .insert(name.to_string(), PerBindingMode { rows, fail: false });
    }

    fn set_fail(&self, name: &str, fail: bool) {
        let mut lock = self.bindings.lock().unwrap();
        let mode = lock.get_mut(name).expect("binding must be registered first");
        mode.fail = fail;
    }

    fn set_rebuild_barrier(&self, barrier: Arc<tokio::sync::Barrier>) {
        *self.rebuild_barrier.lock().unwrap() = Some(barrier);
    }
}

#[async_trait]
impl Source for ConfigurableFakeSource {
    async fn stream_rows<'a>(
        &'a self,
        binding: &'a PortBinding,
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        let key = binding.collection.as_str().to_string();
        let (rows, fail) = {
            let lock = self.bindings.lock().unwrap();
            let mode = lock.get(&key).cloned_or_missing(&key)?;
            (mode.rows, mode.fail)
        };
        let barrier = self.rebuild_barrier.lock().unwrap().clone();
        if let Some(b) = barrier {
            b.wait().await;
        }
        if fail {
            return Err(SourceError::backend_msg("injected", "test fault"));
        }
        let mut sorted = rows;
        sorted.sort_by_key(|r| r.feature_id);
        Ok(Box::pin(stream::iter(sorted.into_iter().map(Ok))))
    }

    async fn stream_rows_by_id<'a>(
        &'a self,
        binding: &'a PortBinding,
        ids: &'a [i64],
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        let key = binding.collection.as_str().to_string();
        let lock = self.bindings.lock().unwrap();
        let mode = lock.get(&key).cloned_or_missing(&key)?;
        if mode.fail {
            return Err(SourceError::backend_msg("injected", "test fault"));
        }
        let by_id: HashMap<u64, RowBytes> = mode.rows.iter().map(|r| (r.feature_id, r.clone())).collect();
        let owned: Vec<RowBytes> = ids.iter().filter_map(|i| by_id.get(&(*i as u64)).cloned()).collect();
        Ok(Box::pin(stream::iter(owned.into_iter().map(Ok))))
    }

    async fn stream_feature_ids<'a>(
        &'a self,
        binding: &'a PortBinding,
    ) -> Result<BoxStream<'a, Result<i64, SourceError>>, SourceError> {
        let key = binding.collection.as_str().to_string();
        let lock = self.bindings.lock().unwrap();
        let mode = lock.get(&key).cloned_or_missing(&key)?;
        let mut ids: Vec<i64> = mode.rows.iter().map(|r| r.feature_id as i64).collect();
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

trait LookupOrMissing<'a, V: Clone> {
    fn cloned_or_missing(self, key: &str) -> Result<V, SourceError>;
}

impl<'a, V: Clone> LookupOrMissing<'a, V> for Option<&'a V> {
    fn cloned_or_missing(self, key: &str) -> Result<V, SourceError> {
        self.cloned()
            .ok_or_else(|| SourceError::backend_msg("unknown binding", key.to_string()))
    }
}

impl Clone for PerBindingMode {
    fn clone(&self) -> Self {
        Self {
            rows: self.rows.clone(),
            fail: self.fail,
        }
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

fn binding_plan(id: &str, page_size: u64) -> BindingPlan {
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
        page_size_target_bytes: page_size,
        sidecar_size_warn_bytes: u64::MAX,
        reconcile_every_cycles: 24,
        simplifier: mars_config::SimplifierKind::Naive,
        missing_page_policy: mars_config::MissingPagePolicy::Truncate,
        dsn: None,    }
}

fn make_deps(source: Arc<ConfigurableFakeSource>) -> (Deps, Arc<InMemoryStore>) {
    let store = Arc::new(InMemoryStore::new());
    let manifest_store = Arc::new(InMemoryPublisher::new());
    let mut registry = mars_compiler::SourceRegistry::new();
    registry.insert(mars_config::SourceId::new("default"), source);
    let deps = Deps {
        sources: Arc::new(registry),
        change_feed: Arc::new(NopFeed),
        leader_lock: Arc::new(NopLock),
        store: store.clone(),
        manifest: manifest_store,
        metrics: Metrics::new().unwrap(),
    };
    (deps, store)
}

/// build a 3-binding plan and bootstrap a manifest from a clean source.
/// each binding gets a single feature so bootstrap is fast; the rebuild
/// tests then drive truncate-class rebuilds that re-derive each binding
/// from the same source.
async fn bootstrap_three_bindings() -> (Arc<ConfigurableFakeSource>, Deps, BootstrapPlan, mars_types::Manifest) {
    let source = Arc::new(ConfigurableFakeSource::default());
    source.add_binding("a", vec![row(1, 10.0, 10.0)]);
    source.add_binding("b", vec![row(2, 20.0, 20.0)]);
    source.add_binding("c", vec![row(3, 30.0, 30.0)]);

    let (deps, _store) = make_deps(source.clone());

    let plan = BootstrapPlan {
        bindings: vec![
            binding_plan("a", 1024),
            binding_plan("b", 1024),
            binding_plan("c", 1024),
        ],
        layers: vec![],
        raster_layers: vec![],
    };

    let bootstrap = run_snapshot_from_plan(
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
        &mars_compiler::memory_governor::MemoryGovernor::new(u64::MAX),
        &mars_compiler::disk_governor::DiskGovernor::new(u64::MAX),
    )
    .await
    .unwrap();

    (source, deps, plan, bootstrap)
}

fn truncate_dirty_for(ids: &[&str]) -> DirtyPages {
    let mut per_binding = BTreeMap::new();
    for id in ids {
        per_binding.insert(
            BindingId::try_new(*id).unwrap(),
            BindingDirty {
                truncated: true,
                per_level: BTreeMap::new(),
                observed: BTreeSet::new(),
            },
        );
    }
    DirtyPages {
        per_binding,
        warnings: vec![],
        failed: BTreeMap::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cycle_rebuild_runs_bindings_concurrently_when_parallelism_allows() {
    let (source, deps, plan, bootstrap) = bootstrap_three_bindings().await;

    // arm the barrier post-bootstrap. all three concurrent rebuilds must
    // reach `stream_rows` before the barrier releases - sequential
    // execution would deadlock the first binding's wait().
    source.set_rebuild_barrier(Arc::new(tokio::sync::Barrier::new(3)));

    let sidecars: HashMap<BindingId, SidecarReader<'_>> = HashMap::new();
    let dirty = truncate_dirty_for(&["a", "b", "c"]);

    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        rebuild_pages(
            &deps,
            &plan,
            &bootstrap,
            &sidecars,
            dirty,
            TEST_WORKING_SET,
            TEST_PLAN_BUDGET,
            TEST_IN_FLIGHT_BUDGET,
            &std::env::temp_dir(),
            256,
            &mars_compiler::memory_governor::MemoryGovernor::new(u64::MAX),
            &mars_compiler::disk_governor::DiskGovernor::new(u64::MAX),
            mars_config::BindingFailurePolicy::FailCycle,
            3,
        ),
    )
    .await
    .expect("rebuild must complete within timeout - barrier proves concurrency")
    .expect("rebuild must succeed");

    // every binding contributed a refreshed binding metadata entry; this
    // is the lightest assertion that proves all three rebuilds ran.
    let refreshed: std::collections::HashSet<_> = outcome
        .refreshed_bindings
        .iter()
        .map(|b| b.binding_id.as_str().to_string())
        .collect();
    assert!(refreshed.contains("a"), "binding a missing from outcome");
    assert!(refreshed.contains("b"), "binding b missing from outcome");
    assert!(refreshed.contains("c"), "binding c missing from outcome");
}

async fn run_isolate_test_at(parallelism: usize) {
    let (source, deps, plan, bootstrap) = bootstrap_three_bindings().await;

    // rig binding "b" - the middle entry in BTreeMap iteration order - to
    // fail on every source call so the rebuild for "b" propagates an
    // error. surviving bindings "a" and "c" must still contribute.
    source.set_fail("b", true);

    let sidecars: HashMap<BindingId, SidecarReader<'_>> = HashMap::new();
    let dirty = truncate_dirty_for(&["a", "b", "c"]);

    let outcome = rebuild_pages(
        &deps,
        &plan,
        &bootstrap,
        &sidecars,
        dirty,
        TEST_WORKING_SET,
        TEST_PLAN_BUDGET,
        TEST_IN_FLIGHT_BUDGET,
        &std::env::temp_dir(),
        256,
        &mars_compiler::memory_governor::MemoryGovernor::new(u64::MAX),
        &mars_compiler::disk_governor::DiskGovernor::new(u64::MAX),
        mars_config::BindingFailurePolicy::Isolate,
        parallelism,
    )
    .await
    .expect("isolate policy must not return Err to the caller");

    let refreshed: std::collections::HashSet<_> = outcome
        .refreshed_bindings
        .iter()
        .map(|b| b.binding_id.as_str().to_string())
        .collect();
    assert!(
        refreshed.contains("a"),
        "binding a should have published under Isolate (parallelism={parallelism})"
    );
    assert!(
        refreshed.contains("c"),
        "binding c should have published under Isolate (parallelism={parallelism})"
    );
    assert!(
        !refreshed.contains("b"),
        "failing binding b must NOT appear in refreshed_bindings (parallelism={parallelism})"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn isolate_policy_skips_failing_binding_serially() {
    run_isolate_test_at(1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn isolate_policy_skips_failing_binding_under_concurrent_rebuild() {
    run_isolate_test_at(4).await;
}

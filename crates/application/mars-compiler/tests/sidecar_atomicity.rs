//! Step 10: sidecar + manifest commit atomicity under fault injection.
//!
//! The contract: `ManifestStore::current()` always returns either the prior
//! manifest or the new one, never something with dangling references. This
//! test wraps `InMemoryStore` in a `FaultInjectingStore` that fails the Nth
//! `put` with `StoreError::Transient`, runs a rebuild cycle for each
//! `N = 0..put_count`, and asserts:
//!   (a) if rebuild fails: every page referenced by the unchanged manifest
//!       is still readable;
//!   (b) if rebuild succeeds: every page referenced by the new manifest is
//!       readable. partial puts may have orphaned objects, but no manifest
//!       reference points at a missing key.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::stream;
use mars_compiler::incremental::IncrementalCycle;
use mars_compiler::plan::{BindingPlan, BootstrapPlan, LevelPlan};
use mars_compiler::render::rebuild_pages;
use mars_compiler::sidecar::SidecarReader;
use mars_compiler::testing::FullScanCompileSession;
use mars_compiler::{Deps, run_snapshot_from_plan};

const TEST_WORKING_SET: u64 = 4 * 1024 * 1024 * 1024;
const TEST_PLAN_BUDGET: u64 = 8 * 1024 * 1024 * 1024;
const TEST_IN_FLIGHT_BUDGET: u64 = 4 * 1024 * 1024 * 1024;
use mars_observability::Metrics;
use mars_source::{
    AttrValue, ChangeEvent, ChangeFeed, ChangeSubscription, GeometryEnvelope, LeaderLock, LeaderLockGuard, RowBytes,
    Source, SourceBinding as PortBinding, SourceCollectionId, SourceError, SourceRowKey,
};
use mars_store::mem::{InMemoryPublisher, InMemoryStore};
use mars_store::{ManifestStore, ObjectStore, StoreError};
use mars_types::{
    ArtifactKey, BindingId, ContentHash, CrsCode, DecimationLevel, LayerSidecarEntry, Manifest, PageEntry,
};

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

fn envelope(x: f64, y: f64) -> GeometryEnvelope {
    GeometryEnvelope {
        centroid: [x, y],
        bbox: mars_types::Bbox::new(x, y, x, y),
    }
}

#[derive(Default)]
struct FakeSource {
    rows: StdMutex<HashMap<u64, RowBytes>>,
}

impl FakeSource {
    fn with_rows(rows: Vec<RowBytes>) -> Self {
        let map: HashMap<u64, RowBytes> = rows.into_iter().map(|r| (r.feature_id, r)).collect();
        Self {
            rows: StdMutex::new(map),
        }
    }

    fn insert(&self, r: RowBytes) {
        self.rows.lock().unwrap().insert(r.feature_id, r);
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
    ) -> Result<Box<dyn mars_source::CompileSession + 'a>, SourceError> {
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

/// Wraps an inner [`ObjectStore`] and fails the Nth `put` once with a
/// transient error; subsequent calls pass through unchanged.
struct FaultInjectingStore {
    inner: Arc<InMemoryStore>,
    fail_at: StdMutex<Option<u32>>,
    counter: StdMutex<u32>,
}

impl FaultInjectingStore {
    fn new(inner: Arc<InMemoryStore>, fail_at: u32) -> Self {
        Self {
            inner,
            fail_at: StdMutex::new(Some(fail_at)),
            counter: StdMutex::new(0),
        }
    }

    fn passthrough(inner: Arc<InMemoryStore>) -> Self {
        Self {
            inner,
            fail_at: StdMutex::new(None),
            counter: StdMutex::new(0),
        }
    }
}

#[async_trait]
impl ObjectStore for FaultInjectingStore {
    async fn get(&self, key: &ArtifactKey, expected: ContentHash) -> Result<Bytes, StoreError> {
        self.inner.get(key, expected).await
    }

    async fn put(&self, key: &ArtifactKey, body: Bytes) -> Result<ContentHash, StoreError> {
        let n = {
            let mut c = self.counter.lock().unwrap();
            let v = *c;
            *c = c.saturating_add(1);
            v
        };
        let trip = {
            let mut fa = self.fail_at.lock().unwrap();
            match *fa {
                Some(target) if target == n => {
                    *fa = None;
                    true
                }
                _ => false,
            }
        };
        if trip {
            return Err(StoreError::Transient(format!("injected failure at put #{n}")));
        }
        self.inner.put(key, body).await
    }

    async fn delete(&self, key: &ArtifactKey) -> Result<(), StoreError> {
        self.inner.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ArtifactKey>, StoreError> {
        self.inner.list(prefix).await
    }
}

fn binding_plan(id: &str, page_size: u64) -> BindingPlan {
    BindingPlan {
        binding_id: BindingId::try_new(id).unwrap(),
        source_table: id.to_string(),
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
        reconcile_every_cycles: 24,
        simplifier: mars_config::SimplifierKind::Naive,
    }
}

fn make_deps(source: Arc<FakeSource>, store: Arc<dyn ObjectStore>, manifest_store: Arc<dyn ManifestStore>) -> Deps {
    Deps {
        source,
        change_feed: Arc::new(NopFeed),
        leader_lock: Arc::new(NopLock),
        store,
        manifest: manifest_store,
        metrics: Metrics::new().unwrap(),
    }
}

/// merge a rebuild outcome into the prior manifest in the same shape the
/// compiler's cycle entry point uses; inlined here so the test can avoid
/// pulling in a private `merge_manifest` symbol.
fn merge(prior: &Manifest, outcome: &mars_compiler::render::RebuildOutcome, next_version: u64) -> Manifest {
    let replacement_pages: std::collections::HashSet<_> =
        outcome.replacement_pages.iter().map(|p| p.key.clone()).collect();
    let dropped_pages: std::collections::HashSet<_> = outcome.dropped_pages.iter().cloned().collect();
    let replacement_class: std::collections::HashSet<_> = outcome
        .replacement_class_sidecars
        .iter()
        .map(|s| (s.layer_id.clone(), s.page_key.clone()))
        .collect();
    let replacement_label: std::collections::HashSet<_> = outcome
        .replacement_label_sidecars
        .iter()
        .map(|s| (s.layer_id.clone(), s.page_key.clone()))
        .collect();
    let dropped_class: std::collections::HashSet<_> = outcome.dropped_class_sidecars.iter().cloned().collect();
    let dropped_label: std::collections::HashSet<_> = outcome.dropped_label_sidecars.iter().cloned().collect();

    let mut pages: Vec<PageEntry> = prior
        .pages
        .iter()
        .filter(|p| !replacement_pages.contains(&p.key) && !dropped_pages.contains(&p.key))
        .cloned()
        .collect();
    pages.extend(outcome.replacement_pages.iter().cloned());

    let mut class_sidecars: Vec<LayerSidecarEntry> = prior
        .class_sidecars
        .iter()
        .filter(|s| {
            let k = (s.layer_id.clone(), s.page_key.clone());
            !replacement_class.contains(&k) && !dropped_class.contains(&k)
        })
        .cloned()
        .collect();
    class_sidecars.extend(outcome.replacement_class_sidecars.iter().cloned());

    let mut label_sidecars: Vec<LayerSidecarEntry> = prior
        .label_sidecars
        .iter()
        .filter(|s| {
            let k = (s.layer_id.clone(), s.page_key.clone());
            !replacement_label.contains(&k) && !dropped_label.contains(&k)
        })
        .cloned()
        .collect();
    label_sidecars.extend(outcome.replacement_label_sidecars.iter().cloned());

    let refreshed: std::collections::HashSet<BindingId> = outcome
        .refreshed_bindings
        .iter()
        .map(|b| b.binding_id.clone())
        .collect();
    let mut bindings = prior
        .bindings
        .iter()
        .filter(|b| !refreshed.contains(&b.binding_id))
        .cloned()
        .collect::<Vec<_>>();
    bindings.extend(outcome.refreshed_bindings.iter().cloned());

    Manifest {
        format_version: prior.format_version,
        version: next_version,
        service: prior.service.clone(),
        created_at: std::time::SystemTime::now(),
        bindings,
        pages,
        class_sidecars,
        label_sidecars,
        style_artifact: prior.style_artifact.clone(),
        source_version: prior.source_version.clone(),
        epoch: next_version,
    }
}

async fn assert_manifest_consistent(store: &dyn ObjectStore, manifest: &Manifest) {
    for page in &manifest.pages {
        let key = page.key.object_key(&page.content_hash).unwrap();
        store
            .get(&key, page.content_hash)
            .await
            .unwrap_or_else(|_| panic!("manifest references missing page {:?}", page.key));
    }
    for sc in &manifest.class_sidecars {
        let key = sc.object_key().unwrap();
        store
            .get(&key, sc.content_hash)
            .await
            .unwrap_or_else(|_| panic!("manifest references missing class sidecar {:?}", sc.page_key));
    }
    for sc in &manifest.label_sidecars {
        let key = sc.object_key().unwrap();
        store
            .get(&key, sc.content_hash)
            .await
            .unwrap_or_else(|_| panic!("manifest references missing label sidecar {:?}", sc.page_key));
    }
    for b in &manifest.bindings {
        if let Some(entry) = &b.page_membership_sidecar {
            store
                .get(&entry.key, entry.hash)
                .await
                .unwrap_or_else(|_| panic!("manifest references missing sidecar for {:?}", b.binding_id));
        }
    }
}

#[tokio::test]
async fn rebuild_cycle_is_atomic_under_put_fault_injection() {
    // Bootstrap baseline: 60 features in a small page budget so the cycle
    // touches multiple pages + multiple sidecar puts.
    let initial: Vec<RowBytes> = (0..60u64)
        .map(|i| row(i, f64::from(i as u32) * 30.0, f64::from(i as u32) * 30.0))
        .collect();

    // first run: how many `put` calls does a clean cycle issue? we use this
    // as the upper bound on N for fault injection.
    let baseline_puts = {
        let source = Arc::new(FakeSource::with_rows(initial.clone()));
        let raw = Arc::new(InMemoryStore::new());
        let injector = Arc::new(FaultInjectingStore::passthrough(raw.clone()));
        let manifest_store: Arc<dyn ManifestStore> = Arc::new(InMemoryPublisher::new());
        let deps = make_deps(
            source.clone(),
            injector.clone() as Arc<dyn ObjectStore>,
            manifest_store.clone(),
        );

        let plan = BootstrapPlan {
            bindings: vec![binding_plan("points", 1024)],
            layers: vec![],
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
        )
        .await
        .unwrap();
        manifest_store.publish(&bootstrap).await.unwrap();
        let _ = run_one_rebuild_cycle(&deps, &source, &plan, &bootstrap).await.unwrap();
        let counter = injector.counter.lock().unwrap();
        *counter
    };
    assert!(
        baseline_puts >= 4,
        "fixture must exercise at least 4 puts; got {baseline_puts}"
    );

    // For each fault index N in 0..baseline_puts, drive an entire fresh
    // bootstrap + cycle; assert manifest stays consistent.
    for fail_at in 0..baseline_puts {
        let source = Arc::new(FakeSource::with_rows(initial.clone()));
        let raw = Arc::new(InMemoryStore::new());
        let manifest_store: Arc<dyn ManifestStore> = Arc::new(InMemoryPublisher::new());

        // bootstrap is run with a clean store so we always have a valid prior.
        let bootstrap_store: Arc<dyn ObjectStore> = Arc::new(FaultInjectingStore::passthrough(raw.clone()));
        let bootstrap_deps = make_deps(source.clone(), bootstrap_store.clone(), manifest_store.clone());
        let plan = BootstrapPlan {
            bindings: vec![binding_plan("points", 1024)],
            layers: vec![],
        };
        let bootstrap = run_snapshot_from_plan(
            &bootstrap_deps,
            &plan,
            "test".into(),
            1,
            TEST_WORKING_SET,
            TEST_PLAN_BUDGET,
            TEST_IN_FLIGHT_BUDGET,
            1,
        )
        .await
        .unwrap();
        manifest_store.publish(&bootstrap).await.unwrap();

        // now switch to a fault-injecting store for the cycle: this puts the
        // (fail_at)-th call into the failure mode. previous bootstrap puts
        // landed via the passthrough wrapper so they don't count.
        let cycle_store: Arc<dyn ObjectStore> = Arc::new(FaultInjectingStore::new(raw.clone(), fail_at));
        let cycle_deps = make_deps(source.clone(), cycle_store.clone(), manifest_store.clone());
        let result = run_one_rebuild_cycle(&cycle_deps, &source, &plan, &bootstrap).await;

        let current = manifest_store.current().await.unwrap().unwrap();
        match result {
            Ok(new_manifest) => {
                manifest_store.publish(&new_manifest).await.unwrap();
                let after = manifest_store.current().await.unwrap().unwrap();
                assert_eq!(after.version, new_manifest.version);
                assert_manifest_consistent(cycle_store.as_ref(), &after).await;
            }
            Err(_) => {
                // cycle failed: manifest pointer untouched; prior manifest still
                // fully resolvable via the same store.
                assert_eq!(current.version, bootstrap.version);
                assert_manifest_consistent(cycle_store.as_ref(), &current).await;
            }
        }
    }
}

/// drive a single rebuild cycle: insert one new feature, ingest a synthetic
/// Insert event, run rebuild_pages, merge and return the new manifest. does
/// NOT call publish -- the caller decides whether the cycle's outcome lands.
async fn run_one_rebuild_cycle(
    deps: &Deps,
    source: &Arc<FakeSource>,
    plan: &BootstrapPlan,
    prior: &Manifest,
) -> Result<Manifest, mars_compiler::CompilerError> {
    // mutate the source to add a new feature -- something the cycle has to
    // re-emit a page for.
    source.insert(row(9999, 12.0, 12.0));

    // mmap prior sidecar.
    let binding_id = BindingId::try_new("points").unwrap();
    let sidecar_ref = prior
        .bindings
        .iter()
        .find(|b| b.binding_id == binding_id)
        .unwrap()
        .page_membership_sidecar
        .as_ref()
        .unwrap();
    let bytes = deps.store.get(&sidecar_ref.key, sidecar_ref.hash).await?;
    let sidecar = SidecarReader::open(&bytes)?;
    let sidecars = HashMap::from([(binding_id.clone(), sidecar)]);
    let level_meta = HashMap::from([(
        binding_id.clone(),
        prior
            .bindings
            .iter()
            .find(|b| b.binding_id == binding_id)
            .unwrap()
            .levels
            .clone(),
    )]);
    let mut cycle = IncrementalCycle::new(plan, &sidecars, &level_meta);
    cycle.ingest(ChangeEvent::Insert {
        collection: SourceCollectionId::new("points"),
        feature_id: 9999,
        new_envelope: envelope(12.0, 12.0),
    })?;
    let dirty = cycle.finish();

    let outcome = rebuild_pages(
        deps,
        plan,
        prior,
        &sidecars,
        dirty,
        TEST_WORKING_SET,
        TEST_PLAN_BUDGET,
        TEST_IN_FLIGHT_BUDGET,
    )
    .await?;
    Ok(merge(prior, &outcome, prior.version + 1))
}

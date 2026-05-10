//! Step 8b: rebalance recovers under directional drift.
//!
//! Bootstraps a one-page fixture, drives `rebalance_candidates` with a
//! target size below the bootstrap page's actual size, then runs
//! `execute_rebalance` and verifies the resulting page set: original page
//! dropped, fresh pages emitted, union of feature ids preserved, each new
//! page in `[0.5x, 1.5x] * target`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::stream;
use mars_compiler::plan::{BindingPlan, BootstrapPlan, LevelPlan};
use mars_compiler::rebalance::{RebalanceOp, SIZE_HI_FACTOR, SIZE_LO_FACTOR, rebalance_candidates};
use mars_compiler::render::execute_rebalance;
use mars_compiler::sidecar::SidecarReader;
use mars_compiler::testing::FullScanCompileSession;
use mars_compiler::{Deps, run_snapshot_from_plan};

const TEST_WORKING_SET: u64 = 4 * 1024 * 1024 * 1024;
const TEST_PLAN_BUDGET: u64 = 8 * 1024 * 1024 * 1024;
const TEST_IN_FLIGHT_BUDGET: u64 = 4 * 1024 * 1024 * 1024;
use mars_observability::Metrics;
use mars_source::{
    AttrValue, ChangeFeed, ChangeSubscription, LeaderLock, LeaderLockGuard, RowBytes, Source,
    SourceBinding as PortBinding, SourceError, SourceRowKey,
};
use mars_store::ObjectStore;
use mars_store::mem::{InMemoryPublisher, InMemoryStore};
use mars_types::{BindingId, CrsCode, DecimationLevel, PageEntry};

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
    ) -> Result<Box<dyn mars_source::CompileSession + 'a>, SourceError> {
        Ok(FullScanCompileSession::boxed(self, binding))
    }
}

#[derive(Default)]
struct NopChangeFeed;
#[async_trait]
impl ChangeFeed for NopChangeFeed {
    async fn subscribe(&self) -> Result<Box<dyn ChangeSubscription>, SourceError> {
        Err(SourceError::NotImplemented {
            what: "test ChangeFeed",
        })
    }
}

#[derive(Default)]
struct NopLeaderLock;
#[async_trait]
impl LeaderLock for NopLeaderLock {
    async fn try_acquire(&self, _key: i64) -> Result<Option<Box<dyn LeaderLockGuard>>, SourceError> {
        Err(SourceError::NotImplemented {
            what: "test LeaderLock",
        })
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

fn make_deps(source: Arc<FakeSource>) -> (Deps, Arc<InMemoryStore>) {
    let store = Arc::new(InMemoryStore::new());
    let manifest_store = Arc::new(InMemoryPublisher::new());
    let deps = Deps {
        source,
        change_feed: Arc::new(NopChangeFeed),
        leader_lock: Arc::new(NopLeaderLock),
        store: store.clone(),
        manifest: manifest_store,
        metrics: Metrics::new().unwrap(),
    };
    (deps, store)
}

#[tokio::test]
async fn rebalance_candidates_flags_oversize_page() {
    // 200 rows packed into a single page (huge page budget).
    let initial: Vec<RowBytes> = (0..200u64)
        .map(|i| row(i, f64::from(i as u32) * 4.0, f64::from(i as u32) * 4.0))
        .collect();
    let source = Arc::new(FakeSource::with_rows(initial));
    let (deps, _store) = make_deps(source);

    let plan = BootstrapPlan {
        bindings: vec![binding_plan("points", 100 * 1024 * 1024)],
        layers: vec![],
    };
    let manifest = run_snapshot_from_plan(
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
    let binding_id = BindingId::try_new("points").unwrap();
    let level0_pages: Vec<PageEntry> = manifest
        .pages
        .iter()
        .filter(|p| p.key.binding_id == binding_id && p.key.level == DecimationLevel::new(0))
        .cloned()
        .collect();
    assert_eq!(level0_pages.len(), 1, "fixture should produce one page");

    let level0_meta = manifest.bindings[0].levels[0].clone();
    let single = &level0_pages[0];
    // pick a target that puts the lone page above 1.5x → forces split into 4.
    let target = single.size_bytes / 4;
    let ops = rebalance_candidates(&level0_meta, &level0_pages, target);
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        RebalanceOp::Split { page, into } => {
            assert_eq!(page, &single.key);
            assert!(*into >= 4, "expected split into >= 4, got {into}");
        }
        other => panic!("expected split, got {other:?}"),
    }
}

#[tokio::test]
async fn execute_rebalance_split_preserves_feature_ids_and_balances_sizes() {
    let initial: Vec<RowBytes> = (0..200u64)
        .map(|i| row(i, f64::from(i as u32) * 4.0, f64::from(i as u32) * 4.0))
        .collect();
    let source = Arc::new(FakeSource::with_rows(initial));
    let (deps, store) = make_deps(source);

    let plan = BootstrapPlan {
        bindings: vec![binding_plan("points", 100 * 1024 * 1024)],
        layers: vec![],
    };
    let manifest = run_snapshot_from_plan(
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
    let binding_id = BindingId::try_new("points").unwrap();
    let level0_meta = manifest.bindings[0].levels[0].clone();
    let level0_pages: Vec<PageEntry> = manifest
        .pages
        .iter()
        .filter(|p| p.key.binding_id == binding_id && p.key.level == DecimationLevel::new(0))
        .cloned()
        .collect();
    let single = &level0_pages[0];

    // mmap the prior sidecar so the executor can resolve the page's id set.
    let sidecar_ref = manifest.bindings[0].page_membership_sidecar.as_ref().unwrap();
    let sidecar_bytes = store.get(&sidecar_ref.key, sidecar_ref.hash).await.unwrap();
    let sidecar = SidecarReader::open(&sidecar_bytes).unwrap();
    let sidecars = HashMap::from([(binding_id.clone(), sidecar)]);

    let target = single.size_bytes / 4;
    let ops = rebalance_candidates(&level0_meta, &level0_pages, target);
    assert!(!ops.is_empty());

    let outcome = execute_rebalance(&deps, &plan, &manifest, &sidecars, ops, 4 * 1024 * 1024 * 1024)
        .await
        .unwrap();

    // original page dropped; >= 2 replacement pages emitted.
    assert_eq!(outcome.dropped_pages.len(), 1);
    assert_eq!(outcome.dropped_pages[0], single.key);
    assert!(
        outcome.replacement_pages.len() >= 2,
        "expected >= 2 replacement pages, got {}",
        outcome.replacement_pages.len()
    );

    // sum of feature_count across replacements equals the prior page count.
    let after_count: u64 = outcome.replacement_pages.iter().map(|p| p.feature_count).sum();
    assert_eq!(after_count, single.feature_count);

    // every prior feature id resolves in exactly one rebuilt page. With the
    // slot-keyed substrate, walk the geometry index of each replacement page
    // and tally per user_id.
    let mut after_size: u64 = 0;
    let mut hits_per_id: HashMap<u64, u32> = HashMap::new();
    for new_page in &outcome.replacement_pages {
        let key = new_page.key.object_key(&new_page.content_hash).unwrap();
        let bytes = store.get(&key, new_page.content_hash).await.unwrap();
        let reader = mars_artifact::ArtifactReader::open(bytes).unwrap();
        let geom_bytes = reader.section(mars_artifact::SectionKind::GeometryPayload).unwrap();
        for entry in mars_artifact::iter_feature_index(&geom_bytes).unwrap() {
            let entry = entry.unwrap();
            *hits_per_id.entry(entry.user_id).or_insert(0) += 1;
        }
        after_size += new_page.size_bytes;
    }
    for id in 0..200u64 {
        let hits = hits_per_id.get(&id).copied().unwrap_or(0);
        assert_eq!(hits, 1, "feature id {id} appeared in {hits} pages, expected exactly 1");
    }
    // total bytes sum stays approximately equal to the prior single page.
    // tolerate up to 50% inflation from re-encoding overhead per page.
    assert!(
        after_size <= single.size_bytes + single.size_bytes / 2,
        "post-rebalance total ({after_size}) blew up vs prior ({})",
        single.size_bytes
    );

    // every replacement page is in the size band [0.5x, 1.5x] * target.
    let lo = (SIZE_LO_FACTOR * target as f64).floor() as u64;
    let hi = (SIZE_HI_FACTOR * target as f64).ceil() as u64;
    for p in &outcome.replacement_pages {
        assert!(
            p.size_bytes >= lo && p.size_bytes <= hi,
            "page {:?} size_bytes={} out of band [{lo}, {hi}]",
            p.key,
            p.size_bytes
        );
    }
}

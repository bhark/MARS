//! Incremental rebuild deduplicates rows whose hilbert key sits exactly on
//! a page boundary.
//!
//! With inclusive (lo, hi) range filtering, a boundary key matched both
//! adjacent pages and the row was double-counted. the fix attributes each
//! row to exactly one dirty page via `pages_for_key` + lowest-PageId tie-
//! breaker.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::stream;
use mars_compiler::hilbert::key_from_centroid;
use mars_compiler::incremental::{BindingDirty, DirtyPages};
use mars_compiler::plan::{BindingPlan, BootstrapPlan, LevelPlan};
use mars_compiler::render::rebuild_pages;
use mars_compiler::sidecar::SidecarReader;
use mars_observability::Metrics;
use mars_source::{
    AttrValue, CompileSession, LeaderLock, LeaderLockGuard, RowBytes, Source, SourceBinding as PortBinding,
    SourceError, SourceRowKey,
};
use mars_store::mem::{InMemoryPublisher, InMemoryStore};
use mars_types::{
    Bbox, BindingId, BindingMetadata, ContentHash, CrsCode, DecimationLevel, HilbertKey, LevelMetadata, Manifest,
    PageEntry, PageId, PageKey,
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
    async fn stream_rows<'a>(
        &'a self,
        _binding: &'a PortBinding,
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        let mut owned: Vec<RowBytes> = self.rows.lock().unwrap().values().cloned().collect();
        owned.sort_by_key(|r| r.feature_id);
        Ok(Box::pin(stream::iter(owned.into_iter().map(Ok))))
    }

    async fn stream_rows_by_id<'a>(
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
        _binding: &'a PortBinding,
    ) -> Result<Box<dyn CompileSession + 'a>, SourceError> {
        unimplemented!()
    }
}

#[derive(Default)]
struct NopChangeFeed;
#[async_trait]
impl mars_source::ChangeFeed for NopChangeFeed {
    async fn subscribe(&self) -> Result<Box<dyn mars_source::ChangeSubscription>, SourceError> {
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

fn make_deps(source: Arc<FakeSource>) -> (mars_compiler::Deps, Arc<InMemoryStore>) {
    let store = Arc::new(InMemoryStore::new());
    let manifest_store = Arc::new(InMemoryPublisher::new());
    let deps = mars_compiler::Deps {
        source,
        change_feed: Arc::new(NopChangeFeed),
        leader_lock: Arc::new(NopLeaderLock),
        store: store.clone(),
        manifest: manifest_store,
        metrics: Metrics::new().unwrap(),
    };
    (deps, store)
}

fn binding_plan(id: &str) -> BindingPlan {
    BindingPlan {
        binding_id: BindingId::try_new(id).unwrap(),
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
    }
}

#[tokio::test]
async fn boundary_key_row_appears_in_exactly_one_dirty_page() {
    let bbox = Bbox::new(0.0, 0.0, 100.0, 100.0);
    let x = 50.0;
    let y = 50.0;
    let boundary_key = key_from_centroid(x, y, bbox);

    // one feature sitting exactly on the boundary.
    let feature_id: u64 = 42;
    let source = Arc::new(FakeSource::with_rows(vec![row(feature_id, x, y)]));
    let (deps, _store) = make_deps(source.clone());

    let binding_id = BindingId::try_new("test").unwrap();
    let level = DecimationLevel::new(0);

    // prior manifest: two pages whose inclusive ranges both cover boundary_key.
    let level_meta = LevelMetadata {
        level,
        vertex_tolerance_m: 0.0,
        geometry_min_size_m: 0.0,
        label_min_priority: 0,
        page_count: 2,
        hilbert_range_table: vec![
            (HilbertKey::new(0), boundary_key, PageId::new(0)),
            (boundary_key, HilbertKey::max(), PageId::new(1)),
        ],
    };

    let prior_binding = BindingMetadata {
        binding_id: binding_id.clone(),
        source_table: "test".into(),
        native_crs: CrsCode::new("EPSG:25832"),
        feature_count_total: 0,
        combined_bbox: bbox,
        levels: vec![level_meta],
        page_membership_sidecar: None,
    };

    let prior = Manifest {
        format_version: mars_types::MANIFEST_FORMAT_VERSION,
        version: 1,
        service: "test".into(),
        created_at: std::time::SystemTime::now(),
        bindings: vec![prior_binding],
        pages: vec![
            PageEntry {
                key: PageKey {
                    binding_id: binding_id.clone(),
                    level,
                    page_id: PageId::new(0),
                },
                content_hash: ContentHash::zero(),
                spatial_bbox: bbox,
                hilbert_range: (HilbertKey::new(0), boundary_key),
                feature_count: 0,
                size_bytes: 0,
            },
            PageEntry {
                key: PageKey {
                    binding_id: binding_id.clone(),
                    level,
                    page_id: PageId::new(1),
                },
                content_hash: ContentHash::zero(),
                spatial_bbox: bbox,
                hilbert_range: (boundary_key, HilbertKey::max()),
                feature_count: 0,
                size_bytes: 0,
            },
        ],
        class_sidecars: vec![],
        label_sidecars: vec![],
        style_artifact: None,
        image_artifact: None,
        raster_layers: Vec::new(),
        source_version: None,
        epoch: 0,
    };

    let plan = BootstrapPlan {
        bindings: vec![binding_plan("test")],
        layers: vec![],
        raster_layers: vec![],
    };

    let dirty = DirtyPages {
        per_binding: BTreeMap::from([(
            binding_id.clone(),
            BindingDirty {
                truncated: false,
                per_level: BTreeMap::from([(level, BTreeSet::from([PageId::new(0), PageId::new(1)]))]),
                observed: BTreeSet::from([feature_id]),
            },
        )]),
        warnings: vec![],
    };

    let sidecars: HashMap<BindingId, SidecarReader<'_>> = HashMap::new();

    let outcome = rebuild_pages(
        &deps,
        &plan,
        &prior,
        &sidecars,
        dirty,
        4 * 1024 * 1024 * 1024,
        8 * 1024 * 1024 * 1024,
        4 * 1024 * 1024 * 1024,
        &std::env::temp_dir(),
        256,
        &mars_compiler::memory_governor::MemoryGovernor::new(u64::MAX),
        &mars_compiler::disk_governor::DiskGovernor::new(u64::MAX),
    )
    .await
    .unwrap();

    let binding_pages: Vec<&PageEntry> = outcome
        .replacement_pages
        .iter()
        .filter(|p| p.key.binding_id == binding_id)
        .collect();

    let dropped: Vec<&PageKey> = outcome
        .dropped_pages
        .iter()
        .filter(|p| p.binding_id == binding_id)
        .collect();

    assert_eq!(
        binding_pages.len(),
        1,
        "boundary row must land in exactly one replacement page, got {binding_pages:?}"
    );
    assert_eq!(
        binding_pages[0].feature_count, 1,
        "replacement page must contain the single boundary row"
    );
    assert_eq!(
        dropped.len(),
        1,
        "the other dirty page must be dropped because it received no rows"
    );
}

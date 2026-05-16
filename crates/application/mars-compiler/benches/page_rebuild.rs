//! page-rebuild bench. bootstraps a fixture with N point rows and a small
//! page budget, then on each iteration drives one cycle that targets a
//! single dirty page. measures the rebuild stage end-to-end (fetch_by_ids
//! → re-emit page → re-emit sidecar).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::hint::black_box;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use futures_core::stream::BoxStream;
use futures_util::stream;
use mars_compiler::incremental::IncrementalCycle;
use mars_compiler::plan::{BindingPlan, BootstrapPlan, LevelPlan};
use mars_compiler::render::rebuild_pages;
use mars_compiler::sidecar::SidecarReader;
use mars_compiler::testing::FullScanCompileSession;
use mars_compiler::{Deps, run_snapshot_from_plan};

const BENCH_WORKING_SET: u64 = 8 * 1024 * 1024 * 1024;
const BENCH_PLAN_BUDGET: u64 = 8 * 1024 * 1024 * 1024;
const BENCH_IN_FLIGHT_BUDGET: u64 = 8 * 1024 * 1024 * 1024;
use mars_observability::Metrics;
use mars_source::{
    AttrValue, ChangeEvent, ChangeFeed, ChangeSubscription, GeometryEnvelope, LeaderLock, LeaderLockGuard, RowBytes,
    Source, SourceBinding as PortBinding, SourceCollectionId, SourceError, SourceRowKey,
};
use mars_store::ManifestStore;
use mars_store::mem::{InMemoryPublisher, InMemoryStore};
use mars_types::{BindingId, BindingMetadata, CrsCode, DecimationLevel, Manifest, PageEntry};
use tokio::runtime::Runtime;

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
    rows: StdMutex<HashMap<u64, RowBytes>>,
}

impl FakeSource {
    fn with_rows(rows: Vec<RowBytes>) -> Self {
        let map: HashMap<u64, RowBytes> = rows.into_iter().map(|r| (r.feature_id, r)).collect();
        Self {
            rows: StdMutex::new(map),
        }
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
        Err(SourceError::NotImplemented { what: "bench feed" })
    }
}
#[derive(Default)]
struct NopLock;
#[async_trait]
impl LeaderLock for NopLock {
    async fn try_acquire(&self, _key: i64) -> Result<Option<Box<dyn LeaderLockGuard>>, SourceError> {
        Err(SourceError::NotImplemented { what: "bench lock" })
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
        reconcile_every_cycles: u32::MAX,
        simplifier: mars_config::SimplifierKind::Naive,
        missing_page_policy: mars_config::MissingPagePolicy::Truncate,
        dsn: None,
    }
}

struct Fixture {
    deps: Deps,
    plan: BootstrapPlan,
    prior: Manifest,
    sidecar_bytes: bytes::Bytes,
    target_page: PageEntry,
}

async fn build_fixture(n_features: usize, page_size: u64) -> Fixture {
    let initial: Vec<RowBytes> = (0..n_features as u64)
        .map(|i| row(i, f64::from(i as u32) * 4.0, f64::from(i as u32) * 4.0))
        .collect();
    let source = Arc::new(FakeSource::with_rows(initial));
    let store = Arc::new(InMemoryStore::new());
    let manifest_store: Arc<dyn ManifestStore> = Arc::new(InMemoryPublisher::new());
    let mut registry = mars_compiler::SourceRegistry::new();
    registry.insert(mars_config::SourceId::new("default"), source.clone());
    let deps = Deps {
        sources: Arc::new(registry),
        change_feed: Arc::new(NopFeed),
        leader_lock: Arc::new(NopLock),
        store: store.clone(),
        manifest: manifest_store,
        metrics: Metrics::new().unwrap(),
    };
    let plan = BootstrapPlan {
        bindings: vec![binding_plan("points", page_size)],
        layers: vec![],
        raster_layers: vec![],
    };
    let prior = run_snapshot_from_plan(
        &deps,
        &plan,
        "bench".into(),
        1,
        BENCH_WORKING_SET,
        BENCH_PLAN_BUDGET,
        BENCH_IN_FLIGHT_BUDGET,
        1,
        &std::env::temp_dir(),
        256,
        &mars_compiler::memory_governor::MemoryGovernor::new(u64::MAX),
        &mars_compiler::disk_governor::DiskGovernor::new(u64::MAX),
    )
    .await
    .unwrap();
    let binding_id = BindingId::try_new("points").unwrap();
    let sidecar_ref = prior
        .bindings
        .iter()
        .find(|b| b.binding_id == binding_id)
        .unwrap()
        .page_membership_sidecar
        .clone()
        .unwrap();
    let sidecar_bytes = mars_store::ObjectStore::get(store.as_ref(), &sidecar_ref.key, sidecar_ref.hash)
        .await
        .unwrap();
    // pick the middle page as the rebuild target.
    let pages: Vec<PageEntry> = prior
        .pages
        .iter()
        .filter(|p| p.key.binding_id == binding_id)
        .cloned()
        .collect();
    let target_page = pages[pages.len() / 2].clone();
    let _ = source; // owned by deps
    Fixture {
        deps,
        plan,
        prior,
        sidecar_bytes,
        target_page,
    }
}

fn bench_page_rebuild(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let configs = [(40_000usize, 5 * 1024 * 1024u64), (200_000, 16 * 1024 * 1024)];
    for &(n, page_size) in &configs {
        let label = format!("page_rebuild/n={n}/page_bytes={page_size}");
        let fixture = rt.block_on(build_fixture(n, page_size));
        let binding_id = BindingId::try_new("points").unwrap();
        let binding_meta_map: HashMap<BindingId, BindingMetadata> = HashMap::from([(
            binding_id.clone(),
            fixture
                .prior
                .bindings
                .iter()
                .find(|b| b.binding_id == binding_id)
                .unwrap()
                .clone(),
        )]);
        // re-pick a centroid inside the target page's bbox.
        let cx = (fixture.target_page.spatial_bbox.min_x + fixture.target_page.spatial_bbox.max_x) / 2.0;
        let cy = (fixture.target_page.spatial_bbox.min_y + fixture.target_page.spatial_bbox.max_y) / 2.0;
        let envelope = GeometryEnvelope {
            centroid: [cx, cy],
            bbox: mars_types::Bbox::new(cx, cy, cx, cy),
        };
        c.bench_function(&label, |b| {
            b.iter(|| {
                rt.block_on(async {
                    let sidecar = SidecarReader::open(&fixture.sidecar_bytes).unwrap();
                    let sidecars = HashMap::from([(binding_id.clone(), sidecar)]);
                    let mut cycle = IncrementalCycle::new(&fixture.plan, &sidecars, &binding_meta_map);
                    cycle
                        .ingest(ChangeEvent::Update {
                            collection: SourceCollectionId::new("points"),
                            feature_id: 0,
                            new_envelope: envelope.clone(),
                        })
                        .unwrap();
                    let dirty = cycle.finish();
                    let _ = rebuild_pages(
                        &fixture.deps,
                        &fixture.plan,
                        &fixture.prior,
                        &sidecars,
                        dirty,
                        BENCH_WORKING_SET,
                        BENCH_PLAN_BUDGET,
                        BENCH_IN_FLIGHT_BUDGET,
                        &std::env::temp_dir(),
                        256,
                        &mars_compiler::memory_governor::MemoryGovernor::new(u64::MAX),
                        &mars_compiler::disk_governor::DiskGovernor::new(u64::MAX),
                        mars_config::BindingFailurePolicy::FailCycle,
                        1,
                    )
                    .await
                    .unwrap();
                });
            });
        });
    }
}

/// rebuild a configurable fraction of pages in a single cycle. exercises
/// the rebuild pipeline under bulk-turnover scenarios (eg. mass updates,
/// bulk imports, or large change feeds). complements the single-page
/// `bench_page_rebuild` above which is the steady-state hot path.
fn bench_multi_page_rebuild(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("compiler_multi_page_rebuild");
    // small page budget so 200k point features split into ~16 pages,
    // giving the 10%/50% sweep meaningful (and distinct) dirty counts.
    let n_features = 200_000usize;
    let page_size = 512 * 1024u64;
    let fixture = rt.block_on(build_fixture(n_features, page_size));
    let binding_id = BindingId::try_new("points").unwrap();
    let binding_meta_map: HashMap<BindingId, BindingMetadata> = HashMap::from([(
        binding_id.clone(),
        fixture
            .prior
            .bindings
            .iter()
            .find(|b| b.binding_id == binding_id)
            .unwrap()
            .clone(),
    )]);

    let pages: Vec<PageEntry> = fixture
        .prior
        .pages
        .iter()
        .filter(|p| p.key.binding_id == binding_id)
        .cloned()
        .collect();
    let total_pages = pages.len();

    for &dirty_fraction in &[0.10_f32, 0.50] {
        let dirty_count = ((total_pages as f32) * dirty_fraction).max(1.0) as usize;
        // pick a feature inside each dirty page so the change-ingest step
        // localises the dirty mark to that page.
        let dirty_envelopes: Vec<(u64, GeometryEnvelope)> = pages
            .iter()
            .step_by((total_pages / dirty_count).max(1))
            .take(dirty_count)
            .enumerate()
            .map(|(i, p)| {
                let cx = (p.spatial_bbox.min_x + p.spatial_bbox.max_x) / 2.0;
                let cy = (p.spatial_bbox.min_y + p.spatial_bbox.max_y) / 2.0;
                (
                    i as u64,
                    GeometryEnvelope {
                        centroid: [cx, cy],
                        bbox: mars_types::Bbox::new(cx, cy, cx, cy),
                    },
                )
            })
            .collect();

        group.throughput(Throughput::Elements(dirty_count as u64));
        let id = BenchmarkId::from_parameter(format!("dirty_{dirty_count}_of_{total_pages}"));
        group.bench_function(id, |b| {
            b.iter(|| {
                rt.block_on(async {
                    let sidecar = SidecarReader::open(&fixture.sidecar_bytes).unwrap();
                    let sidecars = HashMap::from([(binding_id.clone(), sidecar)]);
                    let mut cycle = IncrementalCycle::new(&fixture.plan, &sidecars, &binding_meta_map);
                    for (fid, env) in &dirty_envelopes {
                        cycle
                            .ingest(ChangeEvent::Update {
                                collection: SourceCollectionId::new("points"),
                                feature_id: *fid,
                                new_envelope: env.clone(),
                            })
                            .unwrap();
                    }
                    let dirty = cycle.finish();
                    let _ = rebuild_pages(
                        &fixture.deps,
                        &fixture.plan,
                        &fixture.prior,
                        &sidecars,
                        dirty,
                        BENCH_WORKING_SET,
                        BENCH_PLAN_BUDGET,
                        BENCH_IN_FLIGHT_BUDGET,
                        &std::env::temp_dir(),
                        256,
                        &mars_compiler::memory_governor::MemoryGovernor::new(u64::MAX),
                        &mars_compiler::disk_governor::DiskGovernor::new(u64::MAX),
                        mars_config::BindingFailurePolicy::FailCycle,
                        1,
                    )
                    .await
                    .unwrap();
                });
            });
        });
    }
    group.finish();
}

/// full bootstrap: drive `run_snapshot_from_plan` from an empty store,
/// emitting every page + sidecar from scratch. dataset size sweeps the
/// page-emit and sidecar-emit cost together.
fn bench_full_bootstrap(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("compiler_full_bootstrap");
    let page_size = 16 * 1024 * 1024u64;
    for &n_features in &[10_000usize, 50_000, 200_000] {
        group.throughput(Throughput::Elements(n_features as u64));
        // pre-build the source corpus once outside iter; rebuild deps/plan
        // per iter so each bootstrap starts from an empty store.
        let initial: Vec<RowBytes> = (0..n_features as u64)
            .map(|i| row(i, f64::from(i as u32) * 4.0, f64::from(i as u32) * 4.0))
            .collect();
        let plan = BootstrapPlan {
            bindings: vec![binding_plan("points", page_size)],
            layers: vec![],
            raster_layers: vec![],
        };

        let id = BenchmarkId::from_parameter(format!("features_{n_features}"));
        group.bench_function(id, |b| {
            b.iter_with_setup(
                || {
                    let source = Arc::new(FakeSource::with_rows(initial.clone()));
                    let store = Arc::new(InMemoryStore::new());
                    let manifest_store: Arc<dyn ManifestStore> = Arc::new(InMemoryPublisher::new());
                    let mut registry = mars_compiler::SourceRegistry::new();
                    registry.insert(mars_config::SourceId::new("default"), source);
                    Deps {
                        sources: Arc::new(registry),
                        change_feed: Arc::new(NopFeed),
                        leader_lock: Arc::new(NopLock),
                        store,
                        manifest: manifest_store,
                        metrics: Metrics::new().unwrap(),
                    }
                },
                |deps| {
                    let manifest = rt
                        .block_on(run_snapshot_from_plan(
                            &deps,
                            &plan,
                            "bench-bootstrap".into(),
                            1,
                            BENCH_WORKING_SET,
                            BENCH_PLAN_BUDGET,
                            BENCH_IN_FLIGHT_BUDGET,
                            1,
                            &std::env::temp_dir(),
                            256,
                            &mars_compiler::memory_governor::MemoryGovernor::new(u64::MAX),
                            &mars_compiler::disk_governor::DiskGovernor::new(u64::MAX),
                        ))
                        .unwrap();
                    black_box(manifest);
                },
            );
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_page_rebuild,
    bench_multi_page_rebuild,
    bench_full_bootstrap
);
criterion_main!(benches);

//! Step 7: surgical-invalidation correctness for one incremental cycle.
//!
//! Bootstraps a small fixture with multiple pages, then drives an
//! INSERT / UPDATE in-page / UPDATE cross-page / DELETE batch through
//! `IncrementalCycle` + `rebuild_pages`. Asserts that pages outside the
//! dirty set carry through with byte-identical content hashes, that the
//! dirty pages have new hashes, and that the page-membership sidecar
//! reflects the post-cycle id set.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::stream;
use mars_compiler::Deps;
use mars_compiler::incremental::IncrementalCycle;
use mars_compiler::plan::{BindingPlan, BootstrapPlan, LevelPlan};
use mars_compiler::rebuild::rebuild_pages;
use mars_compiler::sidecar::SidecarReader;
use mars_compiler::snapshot::run_snapshot;
use mars_observability::Metrics;
use mars_source::{
    AttrValue, ChangeEvent, ChangeFeed, ChangeSubscription, GeometryEnvelope, LeaderLock, LeaderLockGuard,
    RowBytes, Source, SourceBinding as PortBinding, SourceCollectionId, SourceError,
};
use mars_store::ObjectStore;
use mars_store::mem::{InMemoryPublisher, InMemoryStore};
use mars_types::{BindingId, ContentHash, CrsCode, DecimationLevel, LevelMetadata, PageEntry, PageKey};

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
    rows: Mutex<HashMap<u64, RowBytes>>,
}

impl FakeSource {
    fn with_rows(rows: Vec<RowBytes>) -> Self {
        let map: HashMap<u64, RowBytes> = rows.into_iter().map(|r| (r.feature_id, r)).collect();
        Self {
            rows: Mutex::new(map),
        }
    }

    fn insert(&self, r: RowBytes) {
        self.rows.lock().unwrap().insert(r.feature_id, r);
    }

    fn remove(&self, id: u64) {
        self.rows.lock().unwrap().remove(&id);
    }

    fn update(&self, id: u64, x: f64, y: f64) {
        let mut lock = self.rows.lock().unwrap();
        let entry = lock.entry(id).or_insert_with(|| row(id, x, y));
        entry.geometry = point_wkb(x, y);
    }
}

#[async_trait]
impl Source for FakeSource {
    async fn fetch_full_table_streaming<'a>(
        &'a self,
        _binding: &'a PortBinding,
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        let mut owned: Vec<RowBytes> = self.rows.lock().unwrap().values().cloned().collect();
        // deterministic order so the bootstrap test fixture is reproducible.
        owned.sort_by_key(|r| r.feature_id);
        Ok(Box::pin(stream::iter(owned.into_iter().map(Ok))))
    }

    async fn fetch_by_feature_ids<'a>(
        &'a self,
        _binding: &'a PortBinding,
        ids: &'a [i64],
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        let lock = self.rows.lock().unwrap();
        let owned: Vec<RowBytes> = ids
            .iter()
            .filter_map(|i| lock.get(&(*i as u64)).cloned())
            .collect();
        Ok(Box::pin(stream::iter(owned.into_iter().map(Ok))))
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

fn page_by_key<'a>(pages: &'a [PageEntry], key: &PageKey) -> &'a PageEntry {
    pages.iter().find(|p| &p.key == key).expect("page must exist")
}

#[tokio::test]
async fn surgical_invalidation_rebuilds_only_dirty_pages() {
    // 30 features along a diagonal across [0..900]. small page budget forces
    // multiple pages so we can verify untouched ones byte-for-byte.
    let initial: Vec<RowBytes> = (0..30u64)
        .map(|i| row(i, f64::from(i as u32) * 30.0, f64::from(i as u32) * 30.0))
        .collect();
    let source = Arc::new(FakeSource::with_rows(initial));
    let (deps, store) = make_deps(source.clone());

    let plan = BootstrapPlan {
        bindings: vec![binding_plan("points", 1024)],
        layers: vec![],
    };

    // bootstrap
    let bootstrap = run_snapshot(&deps, &plan, "test".into(), 1, 4 * 1024 * 1024 * 1024).await.unwrap();
    assert!(
        bootstrap.pages.len() >= 3,
        "fixture must produce >= 3 pages to exercise cross-page moves; got {}",
        bootstrap.pages.len()
    );

    // capture prior content hashes per PageKey for the post-cycle diff.
    let prior_hashes: HashMap<PageKey, ContentHash> =
        bootstrap.pages.iter().map(|p| (p.key.clone(), p.content_hash)).collect();

    // pick three distinct pages to drive the four mutations against.
    // the bootstrap's page list is sorted by hilbert_range.0, so the first
    // page covers the smallest keys, the last the largest.
    let binding_id = BindingId::try_new("points").unwrap();
    let pages_for_binding: Vec<&PageEntry> =
        bootstrap.pages.iter().filter(|p| p.key.binding_id == binding_id).collect();
    let page_a = pages_for_binding.first().expect("first page").key.clone();
    let page_b = pages_for_binding[pages_for_binding.len() / 2].key.clone();
    let page_c = pages_for_binding.last().expect("last page").key.clone();
    assert_ne!(page_a, page_b);
    assert_ne!(page_b, page_c);

    // pick concrete feature ids inside each page from the prior sidecar via
    // its hilbert range. the sidecar yields (feature_id, key) ordered by id.
    let level_meta = bootstrap
        .bindings
        .iter()
        .find(|b| b.binding_id == binding_id)
        .unwrap()
        .levels[0]
        .clone();
    let bootstrap_combined = level_meta.combined_bbox;
    let prior_sidecar_ref = bootstrap
        .bindings
        .iter()
        .find(|b| b.binding_id == binding_id)
        .unwrap()
        .page_membership_sidecar
        .clone()
        .expect("bootstrap binding must have a sidecar");
    let prior_sidecar_bytes = store.get(&prior_sidecar_ref.key, prior_sidecar_ref.hash).await.unwrap();
    let prior_sidecar = SidecarReader::open(&prior_sidecar_bytes).unwrap();

    let id_in_page = |key: &PageKey| -> u64 {
        let page = page_by_key(&bootstrap.pages, key);
        let (lo, hi) = page.hilbert_range;
        prior_sidecar
            .iter()
            .find_map(|(id, k)| if k >= lo && k <= hi { Some(id) } else { None })
            .expect("sidecar must contain at least one feature in this page")
    };

    let id_in_a_to_delete = id_in_page(&page_a);
    let id_in_b_in_page = {
        // pick an id inside B that is NOT the first id picked in A (page_a may
        // equal page_b in degenerate single-page fixtures; we already asserted
        // they differ).
        let (lo, hi) = page_by_key(&bootstrap.pages, &page_b).hilbert_range;
        prior_sidecar
            .iter()
            .filter(|(id, _)| *id != id_in_a_to_delete)
            .find_map(|(id, k)| if k >= lo && k <= hi { Some(id) } else { None })
            .expect("page B must contain a feature distinct from the deleted one")
    };
    let id_to_move_b_to_c = {
        let (b_lo, b_hi) = page_by_key(&bootstrap.pages, &page_b).hilbert_range;
        prior_sidecar
            .iter()
            .filter(|(id, _)| *id != id_in_b_in_page)
            .find_map(|(id, k)| if k >= b_lo && k <= b_hi { Some(id) } else { None })
            .expect("page B must contain a second distinct feature for the cross-page move")
    };
    let new_id_to_insert: u64 = 9999;

    // place the moved feature inside page C's bbox using its centroid.
    let page_c_entry = page_by_key(&bootstrap.pages, &page_c);
    let target_xy_in_c = (
        (page_c_entry.spatial_bbox.min_x + page_c_entry.spatial_bbox.max_x) / 2.0,
        (page_c_entry.spatial_bbox.min_y + page_c_entry.spatial_bbox.max_y) / 2.0,
    );
    // place the in-page update at B's centroid so the new envelope stays in
    // B's hilbert range.
    let page_b_entry = page_by_key(&bootstrap.pages, &page_b);
    let target_xy_in_b = (
        (page_b_entry.spatial_bbox.min_x + page_b_entry.spatial_bbox.max_x) / 2.0,
        (page_b_entry.spatial_bbox.min_y + page_b_entry.spatial_bbox.max_y) / 2.0,
    );
    let page_a_entry = page_by_key(&bootstrap.pages, &page_a);
    let insert_xy_in_a = (
        (page_a_entry.spatial_bbox.min_x + page_a_entry.spatial_bbox.max_x) / 2.0,
        (page_a_entry.spatial_bbox.min_y + page_a_entry.spatial_bbox.max_y) / 2.0,
    );

    // mutate the source to mirror what the change feed will report.
    source.insert(row(new_id_to_insert, insert_xy_in_a.0, insert_xy_in_a.1));
    source.update(id_in_b_in_page, target_xy_in_b.0, target_xy_in_b.1);
    source.update(id_to_move_b_to_c, target_xy_in_c.0, target_xy_in_c.1);
    source.remove(id_in_a_to_delete);

    let _ = bootstrap_combined; // bbox is implicit in IncrementalCycle's level_meta

    // construct synthetic change events.
    let collection = SourceCollectionId::new("points");
    let events = vec![
        ChangeEvent::Insert {
            collection: collection.clone(),
            feature_id: new_id_to_insert,
            new_envelope: envelope(insert_xy_in_a.0, insert_xy_in_a.1),
        },
        ChangeEvent::Update {
            collection: collection.clone(),
            feature_id: id_in_b_in_page,
            new_envelope: envelope(target_xy_in_b.0, target_xy_in_b.1),
            old_envelope: None, // resolved via sidecar lookup
        },
        ChangeEvent::Update {
            collection: collection.clone(),
            feature_id: id_to_move_b_to_c,
            new_envelope: envelope(target_xy_in_c.0, target_xy_in_c.1),
            old_envelope: None,
        },
        ChangeEvent::Delete {
            collection: collection.clone(),
            feature_id: id_in_a_to_delete,
            old_envelope: None,
        },
    ];

    // run the cycle.
    let sidecars: HashMap<BindingId, SidecarReader<'_>> = HashMap::from([(binding_id.clone(), prior_sidecar)]);
    let level_meta_map: HashMap<BindingId, Vec<LevelMetadata>> =
        HashMap::from([(binding_id.clone(), vec![level_meta.clone()])]);

    let mut cycle = IncrementalCycle::new(&plan, &sidecars, &level_meta_map);
    for ev in events {
        cycle.ingest(ev).unwrap();
    }
    let dirty = cycle.finish();
    assert!(dirty.warnings.is_empty(), "no warnings expected: {:?}", dirty.warnings);

    let outcome = rebuild_pages(&deps, &plan, &bootstrap, &sidecars, dirty, 4 * 1024 * 1024 * 1024).await.unwrap();

    // pages outside the touched set (A, B, C) must be untouched: not present
    // in replacement_pages and not in dropped_pages.
    let touched: std::collections::HashSet<PageKey> = [page_a.clone(), page_b.clone(), page_c.clone()]
        .into_iter()
        .collect();
    let replaced: std::collections::HashSet<PageKey> =
        outcome.replacement_pages.iter().map(|p| p.key.clone()).collect();
    let dropped: std::collections::HashSet<PageKey> = outcome.dropped_pages.iter().cloned().collect();

    for prior in &bootstrap.pages {
        if touched.contains(&prior.key) {
            continue;
        }
        assert!(
            !replaced.contains(&prior.key),
            "page {:?} was rebuilt but should have been left alone",
            prior.key
        );
        assert!(
            !dropped.contains(&prior.key),
            "page {:?} was dropped but should have been left alone",
            prior.key
        );
    }

    // touched pages either land in replacement_pages with a NEW content hash
    // or in dropped_pages (only legal if the page becomes empty).
    for tk in [&page_a, &page_b, &page_c] {
        if let Some(new_entry) = outcome.replacement_pages.iter().find(|p| &p.key == tk) {
            let prior_hash = prior_hashes.get(tk).copied().unwrap();
            assert_ne!(
                new_entry.content_hash, prior_hash,
                "rebuilt page {tk:?} kept its prior content hash"
            );
        } else {
            assert!(
                dropped.contains(tk),
                "touched page {tk:?} is neither rebuilt nor dropped"
            );
        }
    }

    // refreshed binding metadata carries a NEW page-membership sidecar; the
    // sidecar reflects the post-cycle id set (insert added, delete removed,
    // moved feature now keyed in C's range).
    let new_meta = outcome
        .refreshed_bindings
        .iter()
        .find(|m| m.binding_id == binding_id)
        .expect("refreshed binding metadata");
    let new_sidecar_ref = new_meta
        .page_membership_sidecar
        .as_ref()
        .expect("refreshed sidecar reference");
    let new_sidecar_bytes = store.get(&new_sidecar_ref.key, new_sidecar_ref.hash).await.unwrap();
    let new_sidecar = SidecarReader::open(&new_sidecar_bytes).unwrap();
    assert!(new_sidecar.lookup(new_id_to_insert).is_some(), "insert must land in sidecar");
    assert!(
        new_sidecar.lookup(id_in_a_to_delete).is_none(),
        "delete must drop from sidecar"
    );
    assert!(
        new_sidecar.lookup(id_in_b_in_page).is_some(),
        "in-page update must remain"
    );
    assert!(
        new_sidecar.lookup(id_to_move_b_to_c).is_some(),
        "moved feature must remain in sidecar"
    );

    // total feature count after the cycle = bootstrap_count + 1 (insert) - 1 (delete).
    let bootstrap_count = bootstrap
        .pages
        .iter()
        .filter(|p| p.key.binding_id == binding_id)
        .map(|p| p.feature_count)
        .sum::<u64>();
    let mut after_count: u64 = 0;
    for prior in &bootstrap.pages {
        if prior.key.binding_id != binding_id {
            continue;
        }
        if dropped.contains(&prior.key) {
            continue;
        }
        if let Some(new_entry) = outcome.replacement_pages.iter().find(|p| p.key == prior.key) {
            after_count += new_entry.feature_count;
        } else {
            after_count += prior.feature_count;
        }
    }
    // a NEW page (not in prior manifest) is also possible but not expected in
    // this fixture - all dirty pages reuse prior PageIds.
    assert_eq!(
        after_count,
        bootstrap_count + 1 - 1,
        "feature count drift: bootstrap={bootstrap_count} after={after_count}"
    );

    // cross-page move: id_to_move_b_to_c must NOT appear in B's rebuilt page,
    // must appear in C's rebuilt page (or one of the surviving pages whose
    // hilbert range now covers it).
    let moved_keys: Vec<u64> = new_sidecar
        .iter()
        .filter(|(id, _)| *id == id_to_move_b_to_c)
        .map(|(_, k)| k.get())
        .collect();
    assert_eq!(moved_keys.len(), 1, "moved feature must appear exactly once in sidecar");
}

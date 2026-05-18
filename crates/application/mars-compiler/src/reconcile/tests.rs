#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::sidecar::encode_sidecar;
use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::stream;
use mars_observability::Metrics;
use mars_source::{
    ChangeFeed, ChangeSubscription, LeaderLock, LeaderLockGuard, RowBytes, Source, SourceError, SourceRowKey,
};
use mars_store::ManifestStore;
use mars_test_support::port_fakes::{NotImplementedManifestStore, NotImplementedStore};
use mars_types::{CrsCode, DecimationLevel, HilbertKey};
use std::sync::Arc;

use crate::plan::LevelPlan;

fn point_wkb(x: f64, y: f64) -> Bytes {
    let mut v = Vec::with_capacity(21);
    v.push(1);
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&x.to_le_bytes());
    v.extend_from_slice(&y.to_le_bytes());
    Bytes::from(v)
}

struct ReconcileSource {
    source_ids: Vec<i64>,
    rows_for_ids: std::collections::HashMap<u64, Vec<RowBytes>>,
}

#[async_trait]
impl Source for ReconcileSource {
    async fn stream_rows<'a>(
        &'a self,
        _binding: &'a PortBinding,
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        Err(SourceError::NotImplemented {
            what: "test stream_rows",
        })
    }

    async fn stream_rows_by_id<'a>(
        &'a self,
        _binding: &'a PortBinding,
        ids: &'a [i64],
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        let owned: Vec<RowBytes> = ids
            .iter()
            .filter_map(|i| self.rows_for_ids.get(&(*i as u64)).cloned())
            .flatten()
            .collect();
        Ok(Box::pin(stream::iter(owned.into_iter().map(Ok))))
    }

    async fn stream_feature_ids<'a>(
        &'a self,
        _binding: &'a PortBinding,
    ) -> Result<BoxStream<'a, Result<i64, SourceError>>, SourceError> {
        let owned = self.source_ids.clone();
        Ok(Box::pin(stream::iter(owned.into_iter().map(Ok))))
    }
}

#[derive(Default)]
struct NopFeed;
#[async_trait]
impl ChangeFeed for NopFeed {
    async fn subscribe(&self) -> Result<Box<dyn ChangeSubscription>, SourceError> {
        Err(SourceError::NotImplemented { what: "test" })
    }
}
#[derive(Default)]
struct NopLock;
#[async_trait]
impl LeaderLock for NopLock {
    async fn try_acquire(&self, _key: i64) -> Result<Option<Box<dyn LeaderLockGuard>>, SourceError> {
        Err(SourceError::NotImplemented { what: "test" })
    }
}

fn binding_plan() -> BindingPlan {
    BindingPlan {
        binding_id: BindingId::try_new("points").unwrap(),
        source_id: mars_config::SourceId::new("default"),
        source_table: "points".into(),
        filter: None,
        geometry_field: "geom".into(),
        id_field: Some("id".into()),
        attributes: vec![],
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

fn make_deps(source: ReconcileSource) -> Deps {
    let mut registry = crate::SourceRegistry::new();
    registry.insert(mars_config::SourceId::new("default"), Arc::new(source));
    Deps {
        sources: Arc::new(registry),
        change_feed: Arc::new(NopFeed),
        leader_lock: Arc::new(NopLock),
        store: Arc::new(NotImplementedStore),
        manifest: Arc::new(NotImplementedManifestStore) as Arc<dyn ManifestStore>,
        metrics: Metrics::new().unwrap(),
    }
}

#[tokio::test]
async fn reconcile_emits_delete_for_orphan_and_insert_for_missing() {
    let mut sidecar_entries = vec![
        (1u64, HilbertKey::new(10)),
        (2u64, HilbertKey::new(20)),
        (3u64, HilbertKey::new(30)),
    ];
    let bytes = encode_sidecar(&mut sidecar_entries).unwrap();
    let sidecar = SidecarReader::open(&bytes).unwrap();

    let mut rows_for_ids = std::collections::HashMap::new();
    rows_for_ids.insert(
        5u64,
        vec![RowBytes {
            feature_id: 5,
            geometry: point_wkb(50.0, 50.0),
            attributes: vec![],
            row_key: SourceRowKey::ZERO,
        }],
    );
    let source = ReconcileSource {
        source_ids: vec![1, 2, 5], // 3 is orphan; 5 is missing
        rows_for_ids,
    };
    let deps = make_deps(source);

    let outcome = reconcile_binding(&deps, &binding_plan(), &sidecar).await.unwrap();
    assert_eq!(outcome.report.orphan_in_sidecar, vec![(3, 1)]);
    assert_eq!(outcome.report.missing_in_sidecar, vec![(5, 1)]);
    assert_eq!(outcome.synthetic_events.len(), 2);

    let has_delete_3 = outcome
        .synthetic_events
        .iter()
        .any(|e| matches!(e, ChangeEvent::Delete { feature_id: 3, .. }));
    let has_insert_5 = outcome.synthetic_events.iter().any(|e| match e {
        ChangeEvent::Insert {
            feature_id: 5,
            new_envelope,
            ..
        } => new_envelope.centroid == [50.0, 50.0],
        _ => false,
    });
    assert!(has_delete_3, "expected Delete for orphan id 3");
    assert!(has_insert_5, "expected Insert for missing id 5 with point envelope");
}

#[tokio::test]
async fn reconcile_in_sync_yields_no_events() {
    let mut sidecar_entries = vec![(1u64, HilbertKey::new(10)), (2u64, HilbertKey::new(20))];
    let bytes = encode_sidecar(&mut sidecar_entries).unwrap();
    let sidecar = SidecarReader::open(&bytes).unwrap();
    let source = ReconcileSource {
        source_ids: vec![1, 2],
        rows_for_ids: std::collections::HashMap::new(),
    };
    let deps = make_deps(source);
    let outcome = reconcile_binding(&deps, &binding_plan(), &sidecar).await.unwrap();
    assert!(outcome.synthetic_events.is_empty());
    assert!(outcome.report.missing_in_sidecar.is_empty());
    assert!(outcome.report.orphan_in_sidecar.is_empty());
}

#[tokio::test]
async fn reconcile_handles_non_unique_user_ids_with_bag_arithmetic() {
    // sidecar has user_id=7 once, source has it three times (e.g. a row
    // exploded into three parts). reconcile must emit two Inserts so
    // the rebuild path absorbs the extras.
    let mut sidecar_entries = vec![(7u64, HilbertKey::new(70))];
    let bytes = encode_sidecar(&mut sidecar_entries).unwrap();
    let sidecar = SidecarReader::open(&bytes).unwrap();

    let mut rows_for_ids = std::collections::HashMap::new();
    rows_for_ids.insert(
        7u64,
        vec![
            RowBytes {
                feature_id: 7,
                geometry: point_wkb(10.0, 10.0),
                attributes: vec![],
                row_key: SourceRowKey::ZERO,
            },
            RowBytes {
                feature_id: 7,
                geometry: point_wkb(20.0, 20.0),
                attributes: vec![],
                row_key: SourceRowKey::ZERO,
            },
            RowBytes {
                feature_id: 7,
                geometry: point_wkb(30.0, 30.0),
                attributes: vec![],
                row_key: SourceRowKey::ZERO,
            },
        ],
    );
    let source = ReconcileSource {
        source_ids: vec![7, 7, 7],
        rows_for_ids,
    };
    let deps = make_deps(source);
    let outcome = reconcile_binding(&deps, &binding_plan(), &sidecar).await.unwrap();
    assert_eq!(outcome.report.missing_in_sidecar, vec![(7u64, 2)]);
    assert!(outcome.report.orphan_in_sidecar.is_empty());
    let inserts: Vec<_> = outcome
        .synthetic_events
        .iter()
        .filter(|e| matches!(e, ChangeEvent::Insert { feature_id: 7, .. }))
        .collect();
    assert_eq!(inserts.len(), 3, "one Insert per source row, not deduped");
}

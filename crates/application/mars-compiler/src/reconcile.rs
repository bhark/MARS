//! periodic page-membership sidecar reconciliation.
//!
//! between cycles the page-membership sidecar can drift from the source if a
//! change-feed message is dropped, the slot lags, or an admin edit slips
//! past the feed. this module compares the binding's source id set against
//! the sidecar's id set, then synthesises [`ChangeEvent`]s that repair the
//! drift on the next incremental cycle: orphans (in sidecar but not source)
//! emit `Delete`, missing rows (in source but not sidecar) fetch geometry
//! once and emit `Insert`.

use std::collections::BTreeSet;

use futures_util::StreamExt;
use mars_artifact::{wkb_centroid, wkb_to_feature_geom};
use mars_source::{ChangeEvent, GeometryEnvelope, SourceBinding as PortBinding, SourceCollectionId};
use mars_types::{Bbox, BindingId};

use crate::plan::BindingPlan;
use crate::sidecar::SidecarReader;
use crate::snapshot::{binding_schema, binding_table};
use crate::{CompilerError, Deps};

/// summary of one reconciliation pass over a binding.
#[derive(Debug, Clone)]
pub struct ReconciliationReport {
    /// binding the pass ran against.
    pub binding_id: BindingId,
    /// ids present in the source but absent from the sidecar.
    pub missing_in_sidecar: Vec<u64>,
    /// ids present in the sidecar but absent from the source.
    pub orphan_in_sidecar: Vec<u64>,
}

/// reconcile output: report plus the synthetic events the caller should
/// feed into the next [`crate::incremental::IncrementalCycle`] to repair
/// drift.
#[derive(Debug, Clone)]
pub struct ReconciliationOutcome {
    /// drift summary. always populated, even when there is nothing to fix.
    pub report: ReconciliationReport,
    /// synthetic [`ChangeEvent`]s that close the drift. empty when in sync.
    pub synthetic_events: Vec<ChangeEvent>,
}

/// run one reconciliation pass over `binding_plan`'s source. streams all
/// source feature ids and diffs against `sidecar`. fetches geometry for
/// any missing ids so the synthetic events can carry envelopes.
pub async fn reconcile_binding(
    deps: &Deps,
    binding_plan: &BindingPlan,
    sidecar: &SidecarReader<'_>,
) -> Result<ReconciliationOutcome, CompilerError> {
    let port_binding = PortBinding::new(
        SourceCollectionId::new(binding_plan.binding_id.as_str()),
        binding_schema(&binding_plan.source_table),
        binding_table(&binding_plan.source_table),
        binding_plan.geometry_column.clone(),
        binding_plan.id_column.as_deref().unwrap_or("id"),
        binding_plan.attributes.clone(),
        binding_plan.native_crs.clone(),
    )?;

    // 1. stream every source feature id.
    let mut stream = deps.source.stream_feature_ids(&port_binding).await?;
    let mut source_ids: BTreeSet<u64> = BTreeSet::new();
    while let Some(item) = stream.next().await {
        let id = item?;
        if id < 0 {
            continue;
        }
        source_ids.insert(id as u64);
    }

    // 2. diff against the sidecar.
    let sidecar_ids: BTreeSet<u64> = sidecar.iter().map(|(id, _)| id).collect();
    let missing: Vec<u64> = source_ids.difference(&sidecar_ids).copied().collect();
    let orphan: Vec<u64> = sidecar_ids.difference(&source_ids).copied().collect();

    let collection = SourceCollectionId::new(binding_plan.binding_id.as_str());
    let mut events: Vec<ChangeEvent> = Vec::with_capacity(missing.len() + orphan.len());

    // 3. orphans: emit Delete with no envelope; the cycle resolves the
    //    hilbert key via sidecar lookup.
    for id in &orphan {
        events.push(ChangeEvent::Delete {
            collection: collection.clone(),
            feature_id: *id,
            old_envelope: None,
        });
    }

    // 4. missing: fetch geometry once so we can emit a real envelope.
    if !missing.is_empty() {
        let ids_signed: Vec<i64> = missing
            .iter()
            .map(|id| i64::try_from(*id).unwrap_or(i64::MAX))
            .collect();
        let mut geom_stream = deps.source.fetch_by_feature_ids(&port_binding, &ids_signed).await?;
        while let Some(item) = geom_stream.next().await {
            let row = item?;
            let envelope = envelope_from_wkb(&row.geometry, row.feature_id)?;
            events.push(ChangeEvent::Insert {
                collection: collection.clone(),
                feature_id: row.feature_id,
                new_envelope: envelope,
            });
        }
    }

    Ok(ReconciliationOutcome {
        report: ReconciliationReport {
            binding_id: binding_plan.binding_id.clone(),
            missing_in_sidecar: missing,
            orphan_in_sidecar: orphan,
        },
        synthetic_events: events,
    })
}

fn envelope_from_wkb(wkb: &[u8], feature_id: u64) -> Result<GeometryEnvelope, CompilerError> {
    let centroid = wkb_centroid(wkb)?;
    let feature = wkb_to_feature_geom(wkb, feature_id)?;
    Ok(GeometryEnvelope {
        centroid,
        bbox: Bbox::new(
            f64::from(feature.bbox[0]),
            f64::from(feature.bbox[1]),
            f64::from(feature.bbox[2]),
            f64::from(feature.bbox[3]),
        ),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sidecar::encode_sidecar;
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures_core::stream::BoxStream;
    use futures_util::stream;
    use mars_observability::Metrics;
    use mars_source::{ChangeFeed, ChangeSubscription, LeaderLock, LeaderLockGuard, RowBytes, Source, SourceError};
    use mars_store::ManifestStore;
    use mars_store::stub::{NotImplementedManifestStore, NotImplementedStore};
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
        rows_for_ids: std::collections::HashMap<u64, RowBytes>,
    }

    #[async_trait]
    impl Source for ReconcileSource {
        async fn fetch_full_table_streaming<'a>(
            &'a self,
            _binding: &'a PortBinding,
        ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
            Err(SourceError::NotImplemented {
                what: "test fetch_full_table_streaming",
            })
        }

        async fn fetch_by_feature_ids<'a>(
            &'a self,
            _binding: &'a PortBinding,
            ids: &'a [i64],
        ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
            let owned: Vec<RowBytes> = ids
                .iter()
                .filter_map(|i| self.rows_for_ids.get(&(*i as u64)).cloned())
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
            source_table: "points".into(),
            geometry_column: "geom".into(),
            id_column: Some("id".into()),
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
        }
    }

    fn make_deps(source: ReconcileSource) -> Deps {
        Deps {
            source: Arc::new(source),
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
            RowBytes {
                feature_id: 5,
                geometry: point_wkb(50.0, 50.0),
                attributes: vec![],
            },
        );
        let source = ReconcileSource {
            source_ids: vec![1, 2, 5], // 3 is orphan; 5 is missing
            rows_for_ids,
        };
        let deps = make_deps(source);

        let outcome = reconcile_binding(&deps, &binding_plan(), &sidecar).await.unwrap();
        assert_eq!(outcome.report.orphan_in_sidecar, vec![3]);
        assert_eq!(outcome.report.missing_in_sidecar, vec![5]);
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
}

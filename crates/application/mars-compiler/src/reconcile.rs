//! periodic page-membership sidecar reconciliation.
//!
//! between cycles the page-membership sidecar can drift from the source if a
//! change-feed message is dropped, the slot lags, or an admin edit slips
//! past the feed. this module compares the binding's source id set against
//! the sidecar's id set, then synthesises [`ChangeEvent`]s that repair the
//! drift on the next incremental cycle: orphans (in sidecar but not source)
//! emit `Delete`, missing rows (in source but not sidecar) fetch geometry
//! once and emit `Insert`.

use std::collections::BTreeMap;

use futures_util::StreamExt;
use mars_artifact::{wkb_centroid, wkb_to_feature_geom};
use mars_source::{ChangeEvent, GeometryEnvelope, SourceBinding as PortBinding, SourceCollectionId};
use mars_types::{Bbox, BindingId};

use crate::plan::BindingPlan;
use crate::sidecar::SidecarReader;
use crate::{CompilerError, Deps};

/// summary of one reconciliation pass over a binding.
///
/// Bag semantics: drift is counted per `user_id` rather than treated as a
/// boolean. `missing_in_sidecar[i] = (user_id, count)` says the source
/// returned `count` more rows for `user_id` than the sidecar carries; the
/// orphan map is the symmetric inverse. With non-unique source ids one
/// row can split into several substrate features, so a single user_id may
/// drift by more than one entry per cycle.
#[derive(Debug, Clone)]
pub struct ReconciliationReport {
    /// binding the pass ran against.
    pub binding_id: BindingId,
    /// `(user_id, count)` pairs for entries present in the source but not
    /// (yet) reflected in the sidecar.
    pub missing_in_sidecar: Vec<(u64, u32)>,
    /// `(user_id, count)` pairs for entries present in the sidecar but no
    /// longer present in the source.
    pub orphan_in_sidecar: Vec<(u64, u32)>,
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
        binding_plan.source_table.clone(),
        binding_plan.geometry_field.clone(),
        binding_plan.id_field.as_deref().unwrap_or("id"),
        binding_plan.attributes.clone(),
        binding_plan.native_crs.clone(),
    )?
    .with_filter(binding_plan.filter.clone());

    // 1. count source rows per user_id (bag, not set - a non-unique source id
    //    contributes one count per row).
    let mut stream = deps.source.stream_feature_ids(&port_binding).await?;
    let mut source_counts: BTreeMap<u64, u32> = BTreeMap::new();
    while let Some(item) = stream.next().await {
        let id = item?;
        if id < 0 {
            continue;
        }
        *source_counts.entry(id as u64).or_default() += 1;
    }

    // 2. count sidecar entries per user_id.
    let mut sidecar_counts: BTreeMap<u64, u32> = BTreeMap::new();
    for (id, _) in sidecar.iter() {
        *sidecar_counts.entry(id).or_default() += 1;
    }

    // 3. diff: every user_id with a non-zero delta drives synthetic events.
    //    union of keys so we can spot ids that left the source entirely.
    let mut all_ids: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    all_ids.extend(source_counts.keys().copied());
    all_ids.extend(sidecar_counts.keys().copied());

    let collection = SourceCollectionId::new(binding_plan.binding_id.as_str());
    let mut missing: Vec<(u64, u32)> = Vec::new();
    let mut orphan: Vec<(u64, u32)> = Vec::new();
    let mut to_fetch: Vec<u64> = Vec::new();
    let mut events: Vec<ChangeEvent> = Vec::new();

    for id in all_ids {
        let src = source_counts.get(&id).copied().unwrap_or(0);
        let side = sidecar_counts.get(&id).copied().unwrap_or(0);
        match src.cmp(&side) {
            std::cmp::Ordering::Greater => {
                missing.push((id, src - side));
                to_fetch.push(id);
            }
            std::cmp::Ordering::Less => {
                let diff = side - src;
                orphan.push((id, diff));
                // emit `diff` Delete events; with bag semantics we cannot
                // pin which specific multimap entry left, so the cycle
                // dirties every page covering any sidecar entry for this
                // user_id (incremental.rs::mark_old_side handles that via
                // sidecar.lookup_all).
                for _ in 0..diff {
                    events.push(ChangeEvent::Delete {
                        collection: collection.clone(),
                        feature_id: id,
                        old_envelope: None,
                    });
                }
            }
            std::cmp::Ordering::Equal => {}
        }
    }

    // 4. missing: fetch geometry for every drifting user_id once and emit
    //    one synthetic Insert per returned row, each carrying its own
    //    envelope so the cycle can identify the dirty page via centroid.
    //    over-emitting beyond the strict deficit is benign - the rebuild
    //    sidecar refresh drops every multimap entry for an observed id
    //    and re-adds entries from the freshly fetched rows, which lands
    //    at parity regardless of the event count.
    if !to_fetch.is_empty() {
        let ids_signed: Vec<i64> = to_fetch
            .iter()
            .map(|id| i64::try_from(*id).unwrap_or(i64::MAX))
            .collect();
        let mut geom_stream = deps.source.stream_rows_by_id(&port_binding, &ids_signed).await?;
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
    use mars_source::{
        ChangeFeed, ChangeSubscription, LeaderLock, LeaderLockGuard, RowBytes, Source, SourceError, SourceRowKey,
    };
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
}

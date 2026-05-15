//! incremental dirty-page identification.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use mars_source::{ChangeEvent, GeometryEnvelope, RebindReason};
use mars_types::{BindingId, BindingMetadata, DecimationLevel, HilbertKey, LevelMetadata, PageId, SourceCollectionId};

use crate::hilbert::key_from_centroid;
use crate::plan::BootstrapPlan;
use crate::sidecar::SidecarReader;

/// dirty pages produced by one incremental cycle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirtyPages {
    /// dirty pages grouped by binding.
    pub per_binding: BTreeMap<BindingId, BindingDirty>,
    /// non-fatal gaps that should be operator-visible.
    pub warnings: Vec<IncrementalWarning>,
    /// bindings the source flagged as degraded for this cycle (failed
    /// preflight on a rebind, dropped from the publication, etc.). the
    /// render dispatch skips their rebuild and the per-binding failure
    /// isolation path preserves their prior pages.
    pub failed: BTreeMap<BindingId, String>,
}

/// dirty state for one binding.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BindingDirty {
    /// full binding rebuild requested.
    pub truncated: bool,
    /// dirty page ids grouped by decimation level.
    pub per_level: BTreeMap<DecimationLevel, BTreeSet<PageId>>,
    /// feature ids observed by this cycle's events. populated for
    /// Insert / Update / Delete; the rebuild path includes these in
    /// `stream_rows_by_id` so newly inserted features land in the
    /// right page even though the page-membership sidecar does not yet
    /// know about them. `Truncate` clears this set since the binding
    /// reverts to the bootstrap path.
    pub observed: BTreeSet<u64>,
}

/// non-fatal incremental-cycle warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncrementalWarning {
    /// old geometry could not be resolved for an update/delete.
    MissingOldGeometry {
        /// affected binding.
        binding_id: BindingId,
        /// affected feature id.
        feature_id: u64,
    },
    /// a hilbert key did not map to any page range.
    MissingPage {
        /// affected binding.
        binding_id: BindingId,
        /// affected level.
        level: DecimationLevel,
        /// unresolved hilbert key.
        key: HilbertKey,
    },
}

/// incremental dirty-page errors.
#[derive(Debug, thiserror::Error)]
pub enum IncrementalError {
    /// change event referenced an unknown collection.
    #[error("incremental: unknown collection {0}")]
    UnknownCollection(String),
    /// no level metadata was supplied for a binding in the plan.
    #[error("incremental: missing level metadata for binding {0}")]
    MissingLevelMetadata(BindingId),
}

/// one pure dirty-page identification cycle.
pub struct IncrementalCycle<'a> {
    plan: &'a BootstrapPlan,
    sidecars: &'a HashMap<BindingId, SidecarReader<'a>>,
    binding_meta: &'a HashMap<BindingId, BindingMetadata>,
    dirty: DirtyPages,
}

impl<'a> IncrementalCycle<'a> {
    /// create a new cycle over a manifest snapshot.
    #[must_use]
    pub fn new(
        plan: &'a BootstrapPlan,
        sidecars: &'a HashMap<BindingId, SidecarReader<'a>>,
        binding_meta: &'a HashMap<BindingId, BindingMetadata>,
    ) -> Self {
        Self {
            plan,
            sidecars,
            binding_meta,
            dirty: DirtyPages::default(),
        }
    }

    /// ingest one source change event.
    pub fn ingest(&mut self, event: ChangeEvent) -> Result<(), IncrementalError> {
        match event {
            ChangeEvent::Insert {
                collection,
                feature_id,
                new_envelope,
            } => {
                let binding_id = self.binding_id_for(&collection)?;
                self.observe(&binding_id, feature_id);
                self.mark_envelope(&binding_id, &new_envelope)?;
            }
            ChangeEvent::Update {
                collection,
                feature_id,
                new_envelope,
                old_envelope,
            } => {
                let binding_id = self.binding_id_for(&collection)?;
                self.observe(&binding_id, feature_id);
                self.mark_envelope(&binding_id, &new_envelope)?;
                self.mark_old_side(&binding_id, feature_id, old_envelope.as_ref())?;
            }
            ChangeEvent::Delete {
                collection,
                feature_id,
                old_envelope,
            } => {
                let binding_id = self.binding_id_for(&collection)?;
                self.observe(&binding_id, feature_id);
                self.mark_old_side(&binding_id, feature_id, old_envelope.as_ref())?;
            }
            ChangeEvent::Truncate { collection } => {
                let binding_id = self.binding_id_for(&collection)?;
                let entry = self.dirty.per_binding.entry(binding_id).or_default();
                entry.truncated = true;
                entry.per_level.clear();
                entry.observed.clear();
            }
            ChangeEvent::Rebind { collection, reason } => {
                let binding_id = self.binding_id_for(&collection)?;
                match reason {
                    RebindReason::OidChanged { .. } => {
                        // same bookkeeping as Truncate: drop accumulated
                        // dirty state and let the cycle re-bootstrap the
                        // binding from a fresh snapshot.
                        let entry = self.dirty.per_binding.entry(binding_id).or_default();
                        entry.truncated = true;
                        entry.per_level.clear();
                        entry.observed.clear();
                    }
                    RebindReason::PreflightFailed { reason } => {
                        self.dirty.failed.insert(binding_id, reason);
                    }
                    RebindReason::BindingUnpublished => {
                        self.dirty
                            .failed
                            .insert(binding_id, "binding absent from publication".to_string());
                    }
                }
            }
        }
        Ok(())
    }

    fn observe(&mut self, binding_id: &BindingId, feature_id: u64) {
        let entry = self.dirty.per_binding.entry(binding_id.clone()).or_default();
        if !entry.truncated {
            entry.observed.insert(feature_id);
        }
    }

    /// finish the cycle.
    #[must_use]
    pub fn finish(self) -> DirtyPages {
        self.dirty
    }

    fn binding_id_for(&self, collection: &SourceCollectionId) -> Result<BindingId, IncrementalError> {
        self.plan
            .bindings
            .iter()
            .find(|binding| binding.binding_id.as_str() == collection.as_str())
            .map(|binding| binding.binding_id.clone())
            .ok_or_else(|| IncrementalError::UnknownCollection(collection.as_str().to_string()))
    }

    fn mark_old_side(
        &mut self,
        binding_id: &BindingId,
        feature_id: u64,
        old_envelope: Option<&GeometryEnvelope>,
    ) -> Result<(), IncrementalError> {
        if let Some(envelope) = old_envelope {
            return self.mark_envelope(binding_id, envelope);
        }
        // sidecar is a multimap on user_id; a single change-feed event
        // covers every part the row exploded into, so dirty every page
        // that any of its sidecar entries touches.
        let keys: Vec<HilbertKey> = self
            .sidecars
            .get(binding_id)
            .map(|sidecar| sidecar.lookup_all(feature_id).collect())
            .unwrap_or_default();
        if keys.is_empty() {
            self.dirty.warnings.push(IncrementalWarning::MissingOldGeometry {
                binding_id: binding_id.clone(),
                feature_id,
            });
            return Ok(());
        }
        for key in keys {
            self.mark_key(binding_id, key)?;
        }
        Ok(())
    }

    fn mark_envelope(&mut self, binding_id: &BindingId, envelope: &GeometryEnvelope) -> Result<(), IncrementalError> {
        let binding = self
            .binding_meta
            .get(binding_id)
            .ok_or_else(|| IncrementalError::MissingLevelMetadata(binding_id.clone()))?;
        let key = key_from_centroid(envelope.centroid[0], envelope.centroid[1], binding.combined_bbox);
        let levels = binding.levels.clone();
        for level in levels {
            self.mark_key_at_level(binding_id, &level, key);
        }
        Ok(())
    }

    fn mark_key(&mut self, binding_id: &BindingId, key: HilbertKey) -> Result<(), IncrementalError> {
        let levels = self
            .binding_meta
            .get(binding_id)
            .ok_or_else(|| IncrementalError::MissingLevelMetadata(binding_id.clone()))?
            .levels
            .clone();
        for level in levels {
            self.mark_key_at_level(binding_id, &level, key);
        }
        Ok(())
    }

    fn mark_key_at_level(&mut self, binding_id: &BindingId, level: &LevelMetadata, key: HilbertKey) {
        if self
            .dirty
            .per_binding
            .get(binding_id)
            .is_some_and(|entry| entry.truncated)
        {
            return;
        }

        let page_ids = pages_for_key(level, key);
        if page_ids.is_empty() {
            self.dirty.warnings.push(IncrementalWarning::MissingPage {
                binding_id: binding_id.clone(),
                level: level.level,
                key,
            });
            return;
        }

        let entry = self.dirty.per_binding.entry(binding_id.clone()).or_default();
        entry.per_level.entry(level.level).or_default().extend(page_ids);
    }
}

pub(crate) fn pages_for_key(level: &LevelMetadata, key: HilbertKey) -> Vec<PageId> {
    // table is sorted ascending by `range_lo`; partition_point gives us the
    // first row whose range starts strictly after `key`. walk back from
    // there collecting any entry whose range still covers `key`. PageId is
    // read from the table - it is not the row index (rebalance allocates
    // fresh ids that no longer match table position).
    let ranges = &level.hilbert_range_table;
    let end = ranges.partition_point(|(range_lo, _, _)| *range_lo <= key);
    let mut page_ids = Vec::new();
    for idx in (0..end).rev() {
        let (range_lo, range_hi, page_id) = ranges[idx];
        if range_hi < key {
            break;
        }
        if range_lo <= key {
            page_ids.push(page_id);
        }
    }
    page_ids
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use mars_source::GeometryEnvelope;
    use mars_types::{Bbox, CrsCode};

    use crate::plan::{BindingPlan, LevelPlan};
    use crate::sidecar::encode_sidecar;

    fn binding(id: &str) -> BindingPlan {
        BindingPlan {
            binding_id: BindingId::try_new(id).unwrap(),
            source_table: id.to_string(),
            filter: None,
            geometry_field: "geom".into(),
            id_field: Some("id".into()),
            attributes: Vec::new(),
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

    fn envelope(x: f64, y: f64) -> GeometryEnvelope {
        GeometryEnvelope {
            centroid: [x, y],
            bbox: Bbox::new(x, y, x, y),
        }
    }

    fn level(level: u8, ranges: Vec<(HilbertKey, HilbertKey)>) -> LevelMetadata {
        // synthesize identity-mapped page ids for the test fixtures; the
        // production code populates them from PageEntry.key.page_id.
        let table = ranges
            .into_iter()
            .enumerate()
            .map(|(i, (lo, hi))| (lo, hi, PageId::new(i as u64)))
            .collect::<Vec<_>>();
        LevelMetadata {
            level: DecimationLevel::new(level),
            vertex_tolerance_m: f64::from(level),
            geometry_min_size_m: 0.0,
            label_min_priority: 0,
            page_count: table.len() as u32,
            hilbert_range_table: table,
        }
    }

    fn binding_meta(id: &str, levels: Vec<LevelMetadata>) -> BindingMetadata {
        BindingMetadata {
            binding_id: BindingId::try_new(id).unwrap(),
            source_table: id.to_string(),
            native_crs: CrsCode::new("EPSG:25832"),
            feature_count_total: 0,
            combined_bbox: Bbox::new(0.0, 0.0, 100.0, 100.0),
            levels,
            page_membership_sidecar: None,
            cycles_since_reconcile: 0,
            last_reconcile_at: None,
        }
    }

    fn exact_ranges(points: &[[f64; 2]]) -> Vec<(HilbertKey, HilbertKey)> {
        let bbox = Bbox::new(0.0, 0.0, 100.0, 100.0);
        let mut keys: Vec<HilbertKey> = points.iter().map(|p| key_from_centroid(p[0], p[1], bbox)).collect();
        keys.sort_unstable();
        keys.dedup();
        keys.into_iter().map(|key| (key, key)).collect()
    }

    #[test]
    fn ingest_marks_dirty_pages_and_truncates_binding() {
        let plan = BootstrapPlan {
            layers: Vec::new(),
            bindings: vec![binding("roads"), binding("buildings")],
            raster_layers: Vec::new(),
        };
        let ranges = exact_ranges(&[[10.0, 10.0], [20.0, 20.0], [30.0, 30.0], [40.0, 40.0], [50.0, 50.0]]);
        let bindings = HashMap::from([
            (
                BindingId::try_new("roads").unwrap(),
                binding_meta("roads", vec![level(0, ranges.clone()), level(1, ranges.clone())]),
            ),
            (
                BindingId::try_new("buildings").unwrap(),
                binding_meta("buildings", vec![level(0, ranges.clone())]),
            ),
        ]);

        let sidecar_key = key_from_centroid(30.0, 30.0, Bbox::new(0.0, 0.0, 100.0, 100.0));
        let mut sidecar_entries = vec![(77, sidecar_key)];
        let sidecar_bytes: Bytes = encode_sidecar(&mut sidecar_entries).unwrap();
        let sidecar = SidecarReader::open(&sidecar_bytes).unwrap();
        let sidecars = HashMap::from([(BindingId::try_new("roads").unwrap(), sidecar)]);

        let mut cycle = IncrementalCycle::new(&plan, &sidecars, &bindings);
        cycle
            .ingest(ChangeEvent::Insert {
                collection: "roads".into(),
                feature_id: 1,
                new_envelope: envelope(10.0, 10.0),
            })
            .unwrap();
        cycle
            .ingest(ChangeEvent::Update {
                collection: "roads".into(),
                feature_id: 2,
                new_envelope: envelope(20.0, 20.0),
                old_envelope: Some(envelope(40.0, 40.0)),
            })
            .unwrap();
        cycle
            .ingest(ChangeEvent::Update {
                collection: "roads".into(),
                feature_id: 77,
                new_envelope: envelope(50.0, 50.0),
                old_envelope: None,
            })
            .unwrap();
        cycle
            .ingest(ChangeEvent::Update {
                collection: "buildings".into(),
                feature_id: 999,
                new_envelope: envelope(10.0, 10.0),
                old_envelope: None,
            })
            .unwrap();
        cycle
            .ingest(ChangeEvent::Delete {
                collection: "roads".into(),
                feature_id: 3,
                old_envelope: Some(envelope(40.0, 40.0)),
            })
            .unwrap();
        cycle
            .ingest(ChangeEvent::Delete {
                collection: "roads".into(),
                feature_id: 77,
                old_envelope: None,
            })
            .unwrap();
        cycle
            .ingest(ChangeEvent::Truncate {
                collection: "buildings".into(),
            })
            .unwrap();

        let dirty = cycle.finish();
        let roads = dirty.per_binding.get(&BindingId::try_new("roads").unwrap()).unwrap();
        assert!(!roads.truncated);
        assert_eq!(
            roads.per_level[&DecimationLevel::new(0)],
            BTreeSet::from_iter((0..5).map(PageId::new))
        );
        assert_eq!(
            roads.per_level[&DecimationLevel::new(1)],
            BTreeSet::from_iter((0..5).map(PageId::new))
        );

        // observed feature ids accumulated for non-truncated bindings.
        assert_eq!(roads.observed, BTreeSet::from_iter([1u64, 2, 3, 77]));

        let buildings = dirty
            .per_binding
            .get(&BindingId::try_new("buildings").unwrap())
            .unwrap();
        assert!(buildings.truncated);
        assert!(buildings.per_level.is_empty());
        // truncate clears observed: bootstrap path supersedes per-feature ids.
        assert!(buildings.observed.is_empty());
        assert_eq!(
            dirty.warnings,
            vec![IncrementalWarning::MissingOldGeometry {
                binding_id: BindingId::try_new("buildings").unwrap(),
                feature_id: 999,
            }]
        );
    }

    #[test]
    fn duplicate_hilbert_key_marks_every_matching_page() {
        let key = HilbertKey::new(42);
        let page_ids = pages_for_key(&level(0, vec![(key, key), (key, key), (key, key)]), key);
        assert_eq!(
            BTreeSet::from_iter(page_ids),
            BTreeSet::from_iter([PageId::new(0), PageId::new(1), PageId::new(2)])
        );
    }

    #[test]
    fn pages_for_key_returns_persisted_page_ids_not_table_index() {
        // simulate a manifest whose page_ids no longer match table position
        // (rebalance allocated fresh ids). pages_for_key must read the
        // persisted page_id, not synthesize from the array index.
        let key_lo = HilbertKey::new(10);
        let key_mid = HilbertKey::new(50);
        let key_hi = HilbertKey::new(90);
        let lvl = LevelMetadata {
            level: DecimationLevel::new(0),
            vertex_tolerance_m: 0.0,
            geometry_min_size_m: 0.0,
            label_min_priority: 0,
            page_count: 3,
            hilbert_range_table: vec![
                (key_lo, key_lo, PageId::new(7)),
                (key_mid, key_mid, PageId::new(42)),
                (key_hi, key_hi, PageId::new(99)),
            ],
        };
        assert_eq!(pages_for_key(&lvl, key_mid), vec![PageId::new(42)]);
        assert_eq!(pages_for_key(&lvl, key_lo), vec![PageId::new(7)]);
        assert_eq!(pages_for_key(&lvl, key_hi), vec![PageId::new(99)]);
        assert!(pages_for_key(&lvl, HilbertKey::new(11)).is_empty());
    }
}

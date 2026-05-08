//! incremental dirty-page identification.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use mars_source::{ChangeEvent, GeometryEnvelope};
use mars_types::{BindingId, DecimationLevel, HilbertKey, LevelMetadata, PageId, SourceCollectionId};

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
}

/// dirty state for one binding.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BindingDirty {
    /// full binding rebuild requested.
    pub truncated: bool,
    /// dirty page ids grouped by decimation level.
    pub per_level: BTreeMap<DecimationLevel, BTreeSet<PageId>>,
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
    level_meta: &'a HashMap<BindingId, Vec<LevelMetadata>>,
    dirty: DirtyPages,
}

impl<'a> IncrementalCycle<'a> {
    /// create a new cycle over a manifest snapshot.
    #[must_use]
    pub fn new(
        plan: &'a BootstrapPlan,
        sidecars: &'a HashMap<BindingId, SidecarReader<'a>>,
        level_meta: &'a HashMap<BindingId, Vec<LevelMetadata>>,
    ) -> Self {
        Self {
            plan,
            sidecars,
            level_meta,
            dirty: DirtyPages::default(),
        }
    }

    /// ingest one source change event.
    pub fn ingest(&mut self, event: ChangeEvent) -> Result<(), IncrementalError> {
        match event {
            ChangeEvent::Insert {
                collection,
                new_envelope,
                ..
            } => {
                let binding_id = self.binding_id_for(&collection)?;
                self.mark_envelope(&binding_id, &new_envelope)?;
            }
            ChangeEvent::Update {
                collection,
                feature_id,
                new_envelope,
                old_envelope,
            } => {
                let binding_id = self.binding_id_for(&collection)?;
                self.mark_envelope(&binding_id, &new_envelope)?;
                self.mark_old_side(&binding_id, feature_id, old_envelope.as_ref())?;
            }
            ChangeEvent::Delete {
                collection,
                feature_id,
                old_envelope,
            } => {
                let binding_id = self.binding_id_for(&collection)?;
                self.mark_old_side(&binding_id, feature_id, old_envelope.as_ref())?;
            }
            ChangeEvent::Truncate { collection } => {
                let binding_id = self.binding_id_for(&collection)?;
                let entry = self.dirty.per_binding.entry(binding_id).or_default();
                entry.truncated = true;
                entry.per_level.clear();
            }
        }
        Ok(())
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
        if let Some(key) = self
            .sidecars
            .get(binding_id)
            .and_then(|sidecar| sidecar.lookup(feature_id))
        {
            return self.mark_key(binding_id, key);
        }
        self.dirty.warnings.push(IncrementalWarning::MissingOldGeometry {
            binding_id: binding_id.clone(),
            feature_id,
        });
        Ok(())
    }

    fn mark_envelope(&mut self, binding_id: &BindingId, envelope: &GeometryEnvelope) -> Result<(), IncrementalError> {
        let levels = self
            .level_meta
            .get(binding_id)
            .ok_or_else(|| IncrementalError::MissingLevelMetadata(binding_id.clone()))?
            .clone();
        for level in levels {
            let key = key_from_centroid(envelope.centroid[0], envelope.centroid[1], level.combined_bbox);
            self.mark_key_at_level(binding_id, &level, key);
        }
        Ok(())
    }

    fn mark_key(&mut self, binding_id: &BindingId, key: HilbertKey) -> Result<(), IncrementalError> {
        let levels = self
            .level_meta
            .get(binding_id)
            .ok_or_else(|| IncrementalError::MissingLevelMetadata(binding_id.clone()))?
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

fn pages_for_key(level: &LevelMetadata, key: HilbertKey) -> Vec<PageId> {
    let ranges = &level.hilbert_range_table;
    let end = ranges.partition_point(|(range_lo, _)| *range_lo <= key);
    let mut page_ids = Vec::new();
    for idx in (0..end).rev() {
        let (range_lo, range_hi) = ranges[idx];
        if range_hi < key {
            break;
        }
        if range_lo <= key {
            page_ids.push(PageId::new(idx as u64));
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
            geometry_column: "geom".into(),
            id_column: Some("id".into()),
            attributes: Vec::new(),
            native_crs: CrsCode::new("EPSG:25832"),
            levels: vec![LevelPlan {
                level: DecimationLevel::new(0),
                vertex_tolerance_m: 0.0,
                geometry_min_size_m: 0.0,
                label_min_priority: 0,
            }],
            page_size_target_bytes: 1024,
        }
    }

    fn envelope(x: f64, y: f64) -> GeometryEnvelope {
        GeometryEnvelope {
            centroid: [x, y],
            bbox: Bbox::new(x, y, x, y),
        }
    }

    fn level(level: u8, ranges: Vec<(HilbertKey, HilbertKey)>) -> LevelMetadata {
        LevelMetadata {
            level: DecimationLevel::new(level),
            vertex_tolerance_m: f64::from(level),
            geometry_min_size_m: 0.0,
            label_min_priority: 0,
            page_count: ranges.len() as u32,
            combined_bbox: Bbox::new(0.0, 0.0, 100.0, 100.0),
            hilbert_range_table: ranges,
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
            bindings: vec![binding("roads"), binding("buildings")],
        };
        let ranges = exact_ranges(&[[10.0, 10.0], [20.0, 20.0], [30.0, 30.0], [40.0, 40.0], [50.0, 50.0]]);
        let levels = HashMap::from([
            (
                BindingId::try_new("roads").unwrap(),
                vec![level(0, ranges.clone()), level(1, ranges.clone())],
            ),
            (BindingId::try_new("buildings").unwrap(), vec![level(0, ranges.clone())]),
        ]);

        let sidecar_key = key_from_centroid(30.0, 30.0, Bbox::new(0.0, 0.0, 100.0, 100.0));
        let mut sidecar_entries = vec![(77, sidecar_key)];
        let sidecar_bytes: Bytes = encode_sidecar(&mut sidecar_entries).unwrap();
        let sidecar = SidecarReader::open(&sidecar_bytes).unwrap();
        let sidecars = HashMap::from([(BindingId::try_new("roads").unwrap(), sidecar)]);

        let mut cycle = IncrementalCycle::new(&plan, &sidecars, &levels);
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

        let buildings = dirty
            .per_binding
            .get(&BindingId::try_new("buildings").unwrap())
            .unwrap();
        assert!(buildings.truncated);
        assert!(buildings.per_level.is_empty());
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
}

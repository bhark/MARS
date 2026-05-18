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
            } => {
                let binding_id = self.binding_id_for(&collection)?;
                self.observe(&binding_id, feature_id);
                self.mark_envelope(&binding_id, &new_envelope)?;
                self.mark_old_side(&binding_id, feature_id)?;
            }
            ChangeEvent::Delete { collection, feature_id } => {
                let binding_id = self.binding_id_for(&collection)?;
                self.observe(&binding_id, feature_id);
                self.mark_old_side(&binding_id, feature_id)?;
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

    fn mark_old_side(&mut self, binding_id: &BindingId, feature_id: u64) -> Result<(), IncrementalError> {
        // sidecar is a multimap on feature_id; a row that exploded into
        // N parts produced N entries in the prior snapshot, so dirty
        // every page that any of its sidecar entries touches. when the
        // feature has no sidecar coverage (e.g. inserted this cycle and
        // not yet folded into a snapshot) we surface a non-fatal warning
        // and move on; the new-side mark already covered the row's
        // current pages.
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
mod tests;

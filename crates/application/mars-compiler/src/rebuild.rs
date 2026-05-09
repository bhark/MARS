//! Phase C.2.c page rebuild executor.
//!
//! Given the dirty set produced by [`crate::incremental::IncrementalCycle`]
//! and the prior manifest, this module fetches the affected feature ids
//! through `Source::fetch_by_feature_ids`, re-decimates per level, and
//! re-emits page artifacts plus class / label sidecars. It also refreshes
//! the per-binding page-membership sidecar to absorb inserts, updates, and
//! deletes from the cycle so the next incremental cycle's old-side lookups
//! resolve correctly.
//!
//! Truncate fall-back: when a binding is marked truncated the rebuild
//! delegates to the snapshot path for that binding only, so the same
//! [`RebuildOutcome`] shape carries both incremental and bootstrap-class
//! work into the manifest commit.
//!
//! Concurrency: this function processes bindings serially. Per-binding
//! parallelism (the LAZARUS "single writer lane per binding") is the cycle
//! entry point's responsibility; here we only require that one rebuild
//! call runs at a time per binding.
//!
//! Sidecar threshold: the encoded page-membership sidecar is checked
//! against the binding's `sidecar_size_warn_bytes`; an exceedance fires a
//! `tracing::warn!` plus a metric counter to surface LAZARUS bailout 4
//! (`REPLICA IDENTITY FULL` mandate) without blocking the cycle.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt;
use mars_artifact::{FeatureGeom, compute_content_hash, wkb_to_feature_geom};
use mars_source::{RowBytes, SourceBinding as PortBinding, SourceCollectionId};
use mars_types::{
    ArtifactEntry, BindingId, BindingMetadata, DecimationLevel, HilbertKey, LayerSidecarEntry, LevelMetadata, Manifest,
    PageEntry, PageId,
};

use crate::decimate::{passes_min_size, simplify};
use crate::external_sort::WorkingSetGuard;
use crate::hilbert::key_from_centroid;
use crate::incremental::{BindingDirty, DirtyPages};
use crate::plan::{BootstrapPlan, LayerPlan, LevelPlan};
use crate::rebalance::RebalanceOp;
use crate::sidecar::{SidecarReader, encode_sidecar};
use crate::snapshot::{
    BindingOutput, KeyedRow, binding_schema, binding_table, emit_layer_sidecars, flush_page,
    membership_sidecar_object_key, snapshot_one_binding, stringify_sidecar_err, stringify_wkb_err,
};
use crate::{CompilerError, Deps};

/// Output of one rebuild pass. Replaces dirty pages and refreshed bindings
/// in the prior manifest; pages and sidecars not listed here carry through
/// unchanged.
#[derive(Debug, Default)]
pub struct RebuildOutcome {
    /// Pages whose content was rewritten this cycle. Keyed by [`PageKey`]
    /// via [`PageEntry::key`]; callers replace any entry in the prior
    /// manifest with the same key.
    pub replacement_pages: Vec<PageEntry>,
    /// Pages that became empty after the rebuild and should be dropped
    /// from the manifest. LAZARUS: "a missing page is a missing page;
    /// no tombstones."
    pub dropped_pages: Vec<mars_types::PageKey>,
    /// Class sidecars rewritten this cycle.
    pub replacement_class_sidecars: Vec<LayerSidecarEntry>,
    /// Label sidecars rewritten this cycle.
    pub replacement_label_sidecars: Vec<LayerSidecarEntry>,
    /// Class sidecars dropped because their page is now empty.
    pub dropped_class_sidecars: Vec<(mars_types::LayerId, mars_types::PageKey)>,
    /// Label sidecars dropped because their page is now empty.
    pub dropped_label_sidecars: Vec<(mars_types::LayerId, mars_types::PageKey)>,
    /// Refreshed binding metadata (level table + new page-membership
    /// sidecar reference). One entry per binding touched by the cycle.
    pub refreshed_bindings: Vec<BindingMetadata>,
}

/// Run one rebuild pass for the given dirty set. Per-binding sidecar
/// thresholds are read from the matching [`BindingPlan`].
pub async fn rebuild_pages(
    deps: &Deps,
    plan: &BootstrapPlan,
    prior: &Manifest,
    sidecars: &HashMap<BindingId, SidecarReader<'_>>,
    dirty: DirtyPages,
    working_set_bytes: u64,
) -> Result<RebuildOutcome, CompilerError> {
    let mut outcome = RebuildOutcome::default();
    for (binding_id, binding_dirty) in dirty.per_binding {
        if binding_dirty.truncated {
            rebuild_binding_truncate(deps, plan, &binding_id, working_set_bytes, &mut outcome).await?;
            continue;
        }
        let sidecar_warn = plan
            .bindings
            .iter()
            .find(|b| b.binding_id == binding_id)
            .map(|b| b.sidecar_size_warn_bytes)
            .unwrap_or(u64::MAX);
        rebuild_binding_incremental(
            deps,
            plan,
            prior,
            sidecars.get(&binding_id),
            &binding_id,
            &binding_dirty,
            working_set_bytes,
            sidecar_warn,
            &mut outcome,
        )
        .await?;
    }
    Ok(outcome)
}

async fn rebuild_binding_truncate(
    deps: &Deps,
    plan: &BootstrapPlan,
    binding_id: &BindingId,
    working_set_bytes: u64,
    outcome: &mut RebuildOutcome,
) -> Result<(), CompilerError> {
    let binding =
        plan.bindings
            .iter()
            .find(|b| b.binding_id == *binding_id)
            .ok_or(CompilerError::LegacySubstrateRetired {
                what: "rebuild: unknown binding for truncate",
            })?;
    let bo: BindingOutput = snapshot_one_binding(deps, binding, plan, working_set_bytes).await?;
    outcome.refreshed_bindings.push(bo.meta);
    outcome.replacement_pages.extend(bo.pages);
    outcome.replacement_class_sidecars.extend(bo.class_sidecars);
    outcome.replacement_label_sidecars.extend(bo.label_sidecars);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn rebuild_binding_incremental(
    deps: &Deps,
    plan: &BootstrapPlan,
    prior: &Manifest,
    sidecar: Option<&SidecarReader<'_>>,
    binding_id: &BindingId,
    binding_dirty: &BindingDirty,
    working_set_bytes: u64,
    sidecar_warn_bytes: u64,
    outcome: &mut RebuildOutcome,
) -> Result<(), CompilerError> {
    let binding_plan =
        plan.bindings
            .iter()
            .find(|b| b.binding_id == *binding_id)
            .ok_or(CompilerError::LegacySubstrateRetired {
                what: "rebuild: unknown binding for incremental cycle",
            })?;
    let prior_binding =
        prior
            .bindings
            .iter()
            .find(|m| m.binding_id == *binding_id)
            .ok_or(CompilerError::LegacySubstrateRetired {
                what: "rebuild: missing prior binding metadata",
            })?;
    let combined_bbox =
        prior_binding
            .levels
            .first()
            .map(|l| l.combined_bbox)
            .ok_or(CompilerError::LegacySubstrateRetired {
                what: "rebuild: prior binding has no level metadata",
            })?;

    // 1. assemble the union of dirty hilbert ranges across all dirty levels.
    //    each (level, page_id) maps into LevelMetadata::hilbert_range_table.
    type DirtyPage = (PageId, (HilbertKey, HilbertKey));
    let mut dirty_ranges: Vec<(HilbertKey, HilbertKey)> = Vec::new();
    let mut dirty_pages_by_level: BTreeMap<DecimationLevel, Vec<DirtyPage>> = BTreeMap::new();
    for (level, page_ids) in &binding_dirty.per_level {
        let level_meta =
            prior_binding
                .levels
                .iter()
                .find(|m| m.level == *level)
                .ok_or(CompilerError::LegacySubstrateRetired {
                    what: "rebuild: missing prior level metadata",
                })?;
        for page_id in page_ids {
            let idx = page_id.get() as usize;
            if let Some(range) = level_meta.hilbert_range_table.get(idx).copied() {
                dirty_ranges.push(range);
                dirty_pages_by_level.entry(*level).or_default().push((*page_id, range));
            }
        }
    }

    // 2. resolve member feature ids: union of (sidecar entries in any dirty
    //    range) and (observed ids from this cycle's events).
    let mut feature_ids: BTreeSet<u64> = BTreeSet::new();
    if let Some(sc) = sidecar {
        for id in sc.feature_ids_in_ranges(&dirty_ranges) {
            feature_ids.insert(id);
        }
    }
    for id in &binding_dirty.observed {
        feature_ids.insert(*id);
    }

    // 3. fetch from source.
    let port_binding = PortBinding::new(
        SourceCollectionId::new(binding_plan.binding_id.as_str()),
        binding_schema(&binding_plan.source_table),
        binding_table(&binding_plan.source_table),
        binding_plan.geometry_column.clone(),
        binding_plan.id_column.as_deref().unwrap_or("id"),
        binding_plan.attributes.clone(),
        binding_plan.native_crs.clone(),
    )?;
    let ids: Vec<i64> = feature_ids
        .iter()
        .map(|f| i64::try_from(*f).unwrap_or(i64::MAX))
        .collect();
    let mut stream = deps.source.fetch_by_feature_ids(&port_binding, &ids).await?;
    let mut guard = WorkingSetGuard::new(working_set_bytes);
    let mut rows: Vec<KeyedRow> = Vec::new();
    let mut returned: BTreeSet<u64> = BTreeSet::new();
    while let Some(item) = stream.next().await {
        let row: RowBytes = item?;
        returned.insert(row.feature_id);
        let geom_bytes_estimate = row.geometry.len() as u64;
        let feature =
            wkb_to_feature_geom(&row.geometry, row.feature_id).map_err(|e| CompilerError::LegacySubstrateRetired {
                what: stringify_wkb_err(&e),
            })?;
        let attr_bytes: u64 = row.attributes.iter().map(|(k, _)| (k.len() + 16) as u64).sum();
        if let Err(observed) = guard.add(geom_bytes_estimate.saturating_add(attr_bytes).saturating_add(64)) {
            return Err(CompilerError::WorkingSetExceeded {
                binding: binding_plan.binding_id.as_str().to_string(),
                observed_bytes: observed,
                ceiling_bytes: working_set_bytes,
            });
        }
        let cx = (f64::from(feature.bbox[0]) + f64::from(feature.bbox[2])) / 2.0;
        let cy = (f64::from(feature.bbox[1]) + f64::from(feature.bbox[3])) / 2.0;
        let key = key_from_centroid(cx, cy, combined_bbox);
        rows.push(KeyedRow {
            feature,
            attrs: Arc::new(row.attributes),
            geom_bytes_estimate,
            key,
        });
    }

    // 4. for every dirty page: filter rows whose key falls inside its prior
    //    hilbert range, re-decimate per the level rules, emit page +
    //    sidecars. empty pages are dropped (no tombstones).
    let layers: Vec<&LevelPlan> = binding_plan.levels.iter().collect();
    let layer_plans: Vec<&crate::plan::LayerPlan> = plan.layers_for(&binding_plan.binding_id).collect();
    for level_plan in &layers {
        let Some(dirty_pages) = dirty_pages_by_level.get(&level_plan.level) else {
            continue;
        };
        for (page_id, (lo, hi)) in dirty_pages {
            let mut page_rows: Vec<KeyedRow> = rows
                .iter()
                .filter(|r| r.key >= *lo && r.key <= *hi)
                .filter(|r| passes_min_size(&r.feature, level_plan.geometry_min_size_m))
                .map(|r| KeyedRow {
                    feature: FeatureGeom {
                        id: r.feature.id,
                        bbox: r.feature.bbox,
                        geom: simplify(&r.feature.geom, level_plan.vertex_tolerance_m, binding_plan.simplifier),
                    },
                    attrs: r.attrs.clone(),
                    geom_bytes_estimate: r.geom_bytes_estimate,
                    key: r.key,
                })
                .collect();
            // page_rows ordering follows row arrival; flush_page reorders by
            // feature_id internally so we don't need to pre-sort here.
            page_rows.sort_by_key(|r| r.key);

            let page_key = mars_types::PageKey {
                binding_id: binding_id.clone(),
                level: level_plan.level,
                page_id: *page_id,
            };
            if page_rows.is_empty() {
                outcome.dropped_pages.push(page_key.clone());
                for layer in &layer_plans {
                    outcome
                        .dropped_class_sidecars
                        .push((layer.layer_id.clone(), page_key.clone()));
                    if layer.label.is_some() {
                        outcome
                            .dropped_label_sidecars
                            .push((layer.layer_id.clone(), page_key.clone()));
                    }
                }
                continue;
            }
            let page_entry = flush_page(deps, binding_plan, level_plan.level, *page_id, &page_rows).await?;
            let mut class_acc: Vec<LayerSidecarEntry> = Vec::new();
            let mut label_acc: Vec<LayerSidecarEntry> = Vec::new();
            emit_layer_sidecars(
                deps,
                level_plan,
                &page_entry,
                &page_rows,
                &layer_plans,
                &mut class_acc,
                &mut label_acc,
            )
            .await?;
            outcome.replacement_pages.push(page_entry);
            outcome.replacement_class_sidecars.append(&mut class_acc);
            outcome.replacement_label_sidecars.append(&mut label_acc);
        }
    }

    // 5. refresh page-membership sidecar: take the prior sidecar entries,
    //    drop every observed id (delete or move), re-add the still-present
    //    feature ids with their newly computed keys.
    let mut new_entries: Vec<(u64, HilbertKey)> = Vec::new();
    if let Some(sc) = sidecar {
        for (id, key) in sc.iter() {
            if !binding_dirty.observed.contains(&id) {
                new_entries.push((id, key));
            }
        }
    }
    for r in &rows {
        // only re-add ids we observed AND that the source returned. inserts
        // and updates land here; deletes drop out because the source did
        // not return them.
        if binding_dirty.observed.contains(&r.feature.id) && returned.contains(&r.feature.id) {
            new_entries.push((r.feature.id, r.key));
        }
    }
    let sidecar_bytes: Bytes = encode_sidecar(&mut new_entries).map_err(|e| CompilerError::LegacySubstrateRetired {
        what: stringify_sidecar_err(&e),
    })?;
    let sidecar_size = sidecar_bytes.len() as u64;
    if sidecar_size > sidecar_warn_bytes {
        tracing::warn!(
            binding = binding_plan.binding_id.as_str(),
            size_bytes = sidecar_size,
            threshold_bytes = sidecar_warn_bytes,
            "page-membership sidecar exceeds warning threshold; consider REPLICA IDENTITY FULL for this binding (LAZARUS bailout 4)"
        );
        deps.metrics
            .inc_compiler_sidecar_threshold_warning(binding_plan.binding_id.as_str());
    }
    let sidecar_hash = compute_content_hash(&sidecar_bytes);
    let sidecar_key = membership_sidecar_object_key(binding_plan.binding_id.as_str(), &sidecar_hash)?;
    deps.store.put(&sidecar_key, sidecar_bytes).await?;

    // 6. compute refreshed level metadata. for now we keep prior level
    //    metadata's combined_bbox + page count, but recompute the hilbert
    //    range table per level by walking the manifest's pages (prior +
    //    replacement set, minus drops). that requires the cycle entry
    //    point to know the union; here we just emit an updated
    //    BindingMetadata with the new sidecar reference and let the cycle
    //    entry point patch the level tables.
    let mut refreshed = prior_binding.clone();
    refreshed.page_membership_sidecar = Some(ArtifactEntry {
        key: sidecar_key,
        hash: sidecar_hash,
        size_bytes: sidecar_size,
    });
    outcome.refreshed_bindings.push(refreshed);
    Ok(())
}

/// Apply a list of [`RebalanceOp`]s, fetching the affected feature ids via
/// `Source::fetch_by_feature_ids` and emitting fresh page artifacts plus
/// class / label sidecars. Source pages are dropped; replacement pages are
/// allocated fresh `PageId`s above the existing maximum at the affected
/// (binding, level). The page-membership sidecar is left untouched -- a
/// rebalance preserves every feature_id and its hilbert key.
pub async fn execute_rebalance(
    deps: &Deps,
    plan: &BootstrapPlan,
    prior: &Manifest,
    sidecars: &HashMap<BindingId, SidecarReader<'_>>,
    ops: Vec<RebalanceOp>,
    working_set_bytes: u64,
) -> Result<RebuildOutcome, CompilerError> {
    let mut outcome = RebuildOutcome::default();
    let mut by_binding: BTreeMap<BindingId, Vec<RebalanceOp>> = BTreeMap::new();
    for op in ops {
        let bid = match &op {
            RebalanceOp::Split { page, .. } => page.binding_id.clone(),
            RebalanceOp::Merge { left, .. } => left.binding_id.clone(),
        };
        by_binding.entry(bid).or_default().push(op);
    }
    for (bid, binding_ops) in by_binding {
        execute_rebalance_one_binding(
            deps,
            plan,
            prior,
            sidecars.get(&bid),
            &bid,
            binding_ops,
            working_set_bytes,
            &mut outcome,
        )
        .await?;
    }
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
async fn execute_rebalance_one_binding(
    deps: &Deps,
    plan: &BootstrapPlan,
    prior: &Manifest,
    sidecar: Option<&SidecarReader<'_>>,
    binding_id: &BindingId,
    ops: Vec<RebalanceOp>,
    working_set_bytes: u64,
    outcome: &mut RebuildOutcome,
) -> Result<(), CompilerError> {
    let binding_plan =
        plan.bindings
            .iter()
            .find(|b| b.binding_id == *binding_id)
            .ok_or(CompilerError::LegacySubstrateRetired {
                what: "rebalance: unknown binding",
            })?;
    let prior_binding =
        prior
            .bindings
            .iter()
            .find(|m| m.binding_id == *binding_id)
            .ok_or(CompilerError::LegacySubstrateRetired {
                what: "rebalance: missing prior binding metadata",
            })?;
    let combined_bbox =
        prior_binding
            .levels
            .first()
            .map(|l| l.combined_bbox)
            .ok_or(CompilerError::LegacySubstrateRetired {
                what: "rebalance: prior binding has no level metadata",
            })?;
    let sc = sidecar.ok_or(CompilerError::LegacySubstrateRetired {
        what: "rebalance: missing page-membership sidecar",
    })?;

    // resolve every page key targeted by these ops, dedup'd.
    let mut source_keys: HashSet<mars_types::PageKey> = HashSet::new();
    for op in &ops {
        match op {
            RebalanceOp::Split { page, .. } => {
                source_keys.insert(page.clone());
            }
            RebalanceOp::Merge { left, right } => {
                source_keys.insert(left.clone());
                source_keys.insert(right.clone());
            }
        }
    }
    let mut source_pages: HashMap<mars_types::PageKey, PageEntry> = HashMap::new();
    for k in &source_keys {
        let entry = prior
            .pages
            .iter()
            .find(|p| &p.key == k)
            .ok_or(CompilerError::LegacySubstrateRetired {
                what: "rebalance: source page missing from prior manifest",
            })?
            .clone();
        source_pages.insert(k.clone(), entry);
    }

    // union of hilbert ranges; sidecar lookup gives us the feature id set.
    let union_ranges: Vec<(HilbertKey, HilbertKey)> = source_pages.values().map(|p| p.hilbert_range).collect();
    let feature_ids = sc.feature_ids_in_ranges(&union_ranges);

    // fetch rows.
    let port_binding = PortBinding::new(
        SourceCollectionId::new(binding_plan.binding_id.as_str()),
        binding_schema(&binding_plan.source_table),
        binding_table(&binding_plan.source_table),
        binding_plan.geometry_column.clone(),
        binding_plan.id_column.as_deref().unwrap_or("id"),
        binding_plan.attributes.clone(),
        binding_plan.native_crs.clone(),
    )?;
    let ids: Vec<i64> = feature_ids
        .iter()
        .map(|f| i64::try_from(*f).unwrap_or(i64::MAX))
        .collect();
    let mut stream = deps.source.fetch_by_feature_ids(&port_binding, &ids).await?;
    let mut guard = WorkingSetGuard::new(working_set_bytes);
    let mut rows: Vec<KeyedRow> = Vec::new();
    while let Some(item) = stream.next().await {
        let row: RowBytes = item?;
        let geom_bytes_estimate = row.geometry.len() as u64;
        let feature =
            wkb_to_feature_geom(&row.geometry, row.feature_id).map_err(|e| CompilerError::LegacySubstrateRetired {
                what: stringify_wkb_err(&e),
            })?;
        let attr_bytes: u64 = row.attributes.iter().map(|(k, _)| (k.len() + 16) as u64).sum();
        if let Err(observed) = guard.add(geom_bytes_estimate.saturating_add(attr_bytes).saturating_add(64)) {
            return Err(CompilerError::WorkingSetExceeded {
                binding: binding_plan.binding_id.as_str().to_string(),
                observed_bytes: observed,
                ceiling_bytes: working_set_bytes,
            });
        }
        let cx = (f64::from(feature.bbox[0]) + f64::from(feature.bbox[2])) / 2.0;
        let cy = (f64::from(feature.bbox[1]) + f64::from(feature.bbox[3])) / 2.0;
        let key = key_from_centroid(cx, cy, combined_bbox);
        rows.push(KeyedRow {
            feature,
            attrs: Arc::new(row.attributes),
            geom_bytes_estimate,
            key,
        });
    }
    rows.sort_by_key(|r| r.key);

    // fresh PageId allocator per affected level.
    let mut next_page_id: HashMap<DecimationLevel, u64> = HashMap::new();
    for level in &prior_binding.levels {
        let max_id = prior
            .pages
            .iter()
            .filter(|p| p.key.binding_id == *binding_id && p.key.level == level.level)
            .map(|p| p.key.page_id.get())
            .max()
            .unwrap_or(0);
        next_page_id.insert(level.level, max_id + 1);
    }

    let layer_plans: Vec<&LayerPlan> = plan.layers_for(binding_id).collect();

    for op in ops {
        match op {
            RebalanceOp::Split { page, into } => {
                let src = source_pages
                    .get(&page)
                    .cloned()
                    .ok_or(CompilerError::LegacySubstrateRetired {
                        what: "rebalance: split source page missing",
                    })?;
                let level_plan = binding_plan.levels.iter().find(|l| l.level == page.level).ok_or(
                    CompilerError::LegacySubstrateRetired {
                        what: "rebalance: split level plan missing",
                    },
                )?;
                let (lo, hi) = src.hilbert_range;
                let in_range: Vec<KeyedRow> = rows.iter().filter(|r| r.key >= lo && r.key <= hi).cloned().collect();
                drop_page_with_sidecars(&page, &layer_plans, outcome);
                if in_range.is_empty() || into == 0 {
                    continue;
                }
                let n = in_range.len();
                let into_usize = into as usize;
                let chunk = n.div_ceil(into_usize);
                for k in 0..into_usize {
                    let start = k * chunk;
                    if start >= n {
                        break;
                    }
                    let end = ((k + 1) * chunk).min(n);
                    let slice: Vec<KeyedRow> = in_range[start..end].to_vec();
                    if slice.is_empty() {
                        continue;
                    }
                    let new_page_id = bump_page_id(&mut next_page_id, page.level);
                    let entry = flush_page(deps, binding_plan, page.level, PageId::new(new_page_id), &slice).await?;
                    let mut class_acc = Vec::new();
                    let mut label_acc = Vec::new();
                    emit_layer_sidecars(
                        deps,
                        level_plan,
                        &entry,
                        &slice,
                        &layer_plans,
                        &mut class_acc,
                        &mut label_acc,
                    )
                    .await?;
                    outcome.replacement_pages.push(entry);
                    outcome.replacement_class_sidecars.append(&mut class_acc);
                    outcome.replacement_label_sidecars.append(&mut label_acc);
                }
            }
            RebalanceOp::Merge { left, right } => {
                let src_l = source_pages
                    .get(&left)
                    .cloned()
                    .ok_or(CompilerError::LegacySubstrateRetired {
                        what: "rebalance: merge left source missing",
                    })?;
                let src_r = source_pages
                    .get(&right)
                    .cloned()
                    .ok_or(CompilerError::LegacySubstrateRetired {
                        what: "rebalance: merge right source missing",
                    })?;
                let level_plan = binding_plan.levels.iter().find(|l| l.level == left.level).ok_or(
                    CompilerError::LegacySubstrateRetired {
                        what: "rebalance: merge level plan missing",
                    },
                )?;
                let (l_lo, l_hi) = src_l.hilbert_range;
                let (r_lo, r_hi) = src_r.hilbert_range;
                let merged: Vec<KeyedRow> = rows
                    .iter()
                    .filter(|r| (r.key >= l_lo && r.key <= l_hi) || (r.key >= r_lo && r.key <= r_hi))
                    .cloned()
                    .collect();
                drop_page_with_sidecars(&left, &layer_plans, outcome);
                drop_page_with_sidecars(&right, &layer_plans, outcome);
                if merged.is_empty() {
                    continue;
                }
                let new_page_id = bump_page_id(&mut next_page_id, left.level);
                let entry = flush_page(deps, binding_plan, left.level, PageId::new(new_page_id), &merged).await?;
                let mut class_acc = Vec::new();
                let mut label_acc = Vec::new();
                emit_layer_sidecars(
                    deps,
                    level_plan,
                    &entry,
                    &merged,
                    &layer_plans,
                    &mut class_acc,
                    &mut label_acc,
                )
                .await?;
                outcome.replacement_pages.push(entry);
                outcome.replacement_class_sidecars.append(&mut class_acc);
                outcome.replacement_label_sidecars.append(&mut label_acc);
            }
        }
    }

    Ok(())
}

fn drop_page_with_sidecars(page: &mars_types::PageKey, layer_plans: &[&LayerPlan], outcome: &mut RebuildOutcome) {
    outcome.dropped_pages.push(page.clone());
    for layer in layer_plans {
        outcome
            .dropped_class_sidecars
            .push((layer.layer_id.clone(), page.clone()));
        if layer.label.is_some() {
            outcome
                .dropped_label_sidecars
                .push((layer.layer_id.clone(), page.clone()));
        }
    }
}

fn bump_page_id(map: &mut HashMap<DecimationLevel, u64>, level: DecimationLevel) -> u64 {
    let next = map.entry(level).or_insert(0);
    let id = *next;
    *next = next.saturating_add(1);
    id
}

/// Recompute level metadata after pages were replaced or dropped. Pure;
/// runs at the cycle entry point after all rebuilds finish, against the
/// merged page list. Exposed here rather than at the cycle entry point
/// because it is the natural complement to [`rebuild_pages`].
#[must_use]
pub fn recompute_level_metadata(prior: &LevelMetadata, pages: &[PageEntry], binding_id: &BindingId) -> LevelMetadata {
    let mut ranges: Vec<(HilbertKey, HilbertKey)> = pages
        .iter()
        .filter(|p| p.key.binding_id == *binding_id && p.key.level == prior.level)
        .map(|p| p.hilbert_range)
        .collect();
    ranges.sort_by_key(|r| r.0);
    LevelMetadata {
        level: prior.level,
        vertex_tolerance_m: prior.vertex_tolerance_m,
        geometry_min_size_m: prior.geometry_min_size_m,
        label_min_priority: prior.label_min_priority,
        page_count: ranges.len() as u32,
        combined_bbox: prior.combined_bbox,
        hilbert_range_table: ranges,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_types::{Bbox, PageKey};

    fn page_entry(binding: &str, level: u8, page_id: u64, lo: u64, hi: u64) -> PageEntry {
        PageEntry {
            key: PageKey {
                binding_id: BindingId::try_new(binding).unwrap(),
                level: DecimationLevel::new(level),
                page_id: PageId::new(page_id),
            },
            content_hash: mars_types::ContentHash::zero(),
            spatial_bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
            hilbert_range: (HilbertKey::new(lo), HilbertKey::new(hi)),
            feature_count: 0,
            size_bytes: 0,
        }
    }

    #[test]
    fn recompute_level_metadata_orders_ranges_and_counts_pages() {
        let prior = LevelMetadata {
            level: DecimationLevel::new(0),
            vertex_tolerance_m: 1.0,
            geometry_min_size_m: 0.0,
            label_min_priority: 0,
            page_count: 0,
            combined_bbox: Bbox::new(0.0, 0.0, 100.0, 100.0),
            hilbert_range_table: vec![],
        };
        let pages = vec![
            page_entry("roads", 0, 0, 100, 200),
            page_entry("roads", 0, 1, 50, 75),
            page_entry("buildings", 0, 0, 0, 999),
            page_entry("roads", 1, 0, 0, 999),
        ];
        let updated = recompute_level_metadata(&prior, &pages, &BindingId::try_new("roads").unwrap());
        assert_eq!(updated.page_count, 2);
        assert_eq!(
            updated.hilbert_range_table,
            vec![
                (HilbertKey::new(50), HilbertKey::new(75)),
                (HilbertKey::new(100), HilbertKey::new(200)),
            ]
        );
    }
}

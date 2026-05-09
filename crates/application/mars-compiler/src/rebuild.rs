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
use futures_core::stream::BoxStream;
use futures_util::StreamExt;
use mars_artifact::{FeatureGeom, compute_content_hash, wkb_to_feature_geom};
use mars_source::{RowBytes, SourceBinding as PortBinding, SourceCollectionId, SourceError};
use mars_types::{
    ArtifactEntry, Bbox, BindingId, BindingMetadata, DecimationLevel, HilbertKey, LayerSidecarEntry, LevelMetadata,
    Manifest, PageEntry, PageId,
};

use crate::decimate::{passes_min_size, simplify};
use crate::external_sort::WorkingSetGuard;
use crate::incremental::{BindingDirty, DirtyPages};
use crate::page_plan::PagePlan;
use crate::plan::{BootstrapPlan, LayerPlan, LevelPlan};
use crate::rebalance::RebalanceOp;
use crate::sidecar::{SidecarReader, encode_sidecar};
use crate::snapshot::{
    BindingOutput, KeyedRow, binding_schema, binding_table, drain_pruned_through, emit_layer_sidecars,
    empty_level_metadata, flush_page, membership_sidecar_object_key, snapshot_one_binding,
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

/// Drain a row stream into deterministic-ordered [`KeyedRow`]s with hilbert
/// keys assigned over `combined_bbox`. Shared by the incremental, rebalance,
/// and (step 6) bootstrap-from-plan paths so all three hydrate rows
/// identically.
///
/// Memory budgets are enforced per-page by the caller (see
/// [`enforce_page_budget`]) — the hydration step itself is unbounded
/// because per-page guards catch single-page outliers and binding-wide
/// pressure is bounded by the feature-id set the caller assembles.
pub(crate) async fn hydrate_keyed_rows<'a>(
    mut stream: BoxStream<'a, Result<RowBytes, SourceError>>,
    combined_bbox: Bbox,
) -> Result<Vec<crate::snapshot::KeyedRow>, CompilerError> {
    let mut rows: Vec<crate::snapshot::KeyedRow> = Vec::new();
    while let Some(item) = stream.next().await {
        let row: RowBytes = item?;
        let geom_bytes_estimate = row.geometry.len() as u64;
        let row_fingerprint = crate::snapshot::compute_row_fingerprint_for_row(&row);
        let feature = wkb_to_feature_geom(&row.geometry, row.feature_id)?;
        let cx = (f64::from(feature.bbox[0]) + f64::from(feature.bbox[2])) / 2.0;
        let cy = (f64::from(feature.bbox[1]) + f64::from(feature.bbox[3])) / 2.0;
        let key = crate::hilbert::key_from_centroid(cx, cy, combined_bbox);
        rows.push(crate::snapshot::KeyedRow {
            feature,
            attrs: Arc::new(row.attributes),
            geom_bytes_estimate,
            key,
            row_fingerprint,
        });
    }
    Ok(rows)
}

/// Sum the working-set bytes of `rows` against `working_set_bytes`. Trips
/// [`CompilerError::ScratchBudgetExceeded`] with `Some(page_id)` when the
/// running total crosses the ceiling. Mirrors the per-row formula
/// [`hydrate_keyed_rows`] used to use, just measured per-page.
pub(crate) fn enforce_page_budget(
    rows: &[crate::snapshot::KeyedRow],
    working_set_bytes: u64,
    binding_id: &str,
    page_id: PageId,
) -> Result<(), CompilerError> {
    let mut guard = WorkingSetGuard::new(working_set_bytes);
    for r in rows {
        let attr_bytes: u64 = r.attrs.iter().map(|(k, _)| (k.len() + 16) as u64).sum();
        let est = r.geom_bytes_estimate.saturating_add(attr_bytes).saturating_add(64);
        if let Err(observed) = guard.add(est) {
            return Err(CompilerError::ScratchBudgetExceeded {
                binding: binding_id.to_string(),
                page_id: Some(page_id),
                observed_bytes: observed,
                budget_bytes: working_set_bytes,
            });
        }
    }
    Ok(())
}

/// Run one rebuild pass for the given dirty set. Per-binding sidecar
/// thresholds are read from the matching [`BindingPlan`].
pub async fn rebuild_pages(
    deps: &Deps,
    plan: &BootstrapPlan,
    prior: &Manifest,
    sidecars: &HashMap<BindingId, SidecarReader<'_>>,
    dirty: DirtyPages,
    spill: &crate::snapshot::SpillConfig,
) -> Result<RebuildOutcome, CompilerError> {
    let mut outcome = RebuildOutcome::default();
    let working_set_bytes = spill.working_set_bytes;
    for (binding_id, binding_dirty) in dirty.per_binding {
        if binding_dirty.truncated {
            rebuild_binding_truncate(deps, plan, &binding_id, spill, &mut outcome).await?;
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
    spill: &crate::snapshot::SpillConfig,
    outcome: &mut RebuildOutcome,
) -> Result<(), CompilerError> {
    let binding =
        plan.bindings
            .iter()
            .find(|b| b.binding_id == *binding_id)
            .ok_or(CompilerError::InvariantViolation {
                what: "rebuild: unknown binding for truncate",
            })?;
    let bo: BindingOutput = snapshot_one_binding(deps, binding, plan, spill).await?;
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
            .ok_or(CompilerError::InvariantViolation {
                what: "rebuild: unknown binding for incremental cycle",
            })?;
    let prior_binding =
        prior
            .bindings
            .iter()
            .find(|m| m.binding_id == *binding_id)
            .ok_or(CompilerError::InvariantViolation {
                what: "rebuild: missing prior binding metadata",
            })?;
    let combined_bbox =
        prior_binding
            .levels
            .first()
            .map(|l| l.combined_bbox)
            .ok_or(CompilerError::InvariantViolation {
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
                .ok_or(CompilerError::InvariantViolation {
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
        for id in sc.user_ids_in_ranges(&dirty_ranges) {
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
    let stream = deps.source.fetch_by_feature_ids(&port_binding, &ids).await?;
    let rows = hydrate_keyed_rows(stream, combined_bbox).await?;
    let mut returned_counts: BTreeMap<u64, u32> = BTreeMap::new();
    for r in &rows {
        *returned_counts.entry(r.feature.user_id).or_default() += 1;
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
            // partition rows in this page's hilbert range into leveled
            // (passes min-size; rendered + paged) and pruned (fails min-size;
            // contributes Independent label candidates only).
            let mut page_rows: Vec<KeyedRow> = Vec::new();
            let mut pruned_rows: Vec<KeyedRow> = Vec::new();
            for r in rows.iter().filter(|r| r.key >= *lo && r.key <= *hi) {
                if passes_min_size(&r.feature, level_plan.geometry_min_size_m) {
                    page_rows.push(KeyedRow {
                        feature: FeatureGeom {
                            user_id: r.feature.user_id,
                            bbox: r.feature.bbox,
                            geom: simplify(&r.feature.geom, level_plan.vertex_tolerance_m, binding_plan.simplifier),
                        },
                        attrs: r.attrs.clone(),
                        geom_bytes_estimate: r.geom_bytes_estimate,
                        key: r.key,
                        row_fingerprint: r.row_fingerprint,
                    });
                } else {
                    pruned_rows.push(r.clone());
                }
            }
            // page_rows must arrive in deterministic slot order: flush_page
            // walks the slice positionally and the position becomes the
            // substrate primary key. matches the bootstrap ordering.
            page_rows.sort_by(|a, b| {
                a.key
                    .cmp(&b.key)
                    .then_with(|| a.feature.user_id.cmp(&b.feature.user_id))
                    .then_with(|| a.row_fingerprint.cmp(&b.row_fingerprint))
            });
            // per-page working-set ceiling. checked over the leveled rows
            // that actually flow into flush_page; pruned rows live on the
            // pruned label sidecar tail and are intentionally excluded.
            enforce_page_budget(
                &page_rows,
                working_set_bytes,
                binding_plan.binding_id.as_str(),
                *page_id,
            )?;

            let page_key = mars_types::PageKey {
                binding_id: binding_id.clone(),
                level: level_plan.level,
                page_id: *page_id,
            };
            if page_rows.is_empty() {
                // no rendered geometry survives at this level for this page.
                // pruned-feature labels have nowhere to attach (no page key),
                // so the whole page including its label sidecars drops.
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
                &pruned_rows,
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

    // 5. refresh page-membership sidecar (multimap on user_id). drop every
    //    observed user_id from the prior sidecar, then re-add one entry per
    //    row the source actually returned. with bag semantics this folds
    //    inserts, updates, and deletes uniformly: a user_id whose source
    //    count drops to zero leaves the sidecar entirely; one whose count
    //    grows accumulates new entries; rebalanced (geometry moved without
    //    a new row count) cases land back at parity.
    let mut new_entries: Vec<(u64, HilbertKey)> = Vec::new();
    if let Some(sc) = sidecar {
        for (id, key) in sc.iter() {
            if !binding_dirty.observed.contains(&id) {
                new_entries.push((id, key));
            }
        }
    }
    for r in &rows {
        if binding_dirty.observed.contains(&r.feature.user_id)
            && returned_counts.get(&r.feature.user_id).copied().unwrap_or(0) > 0
        {
            new_entries.push((r.feature.user_id, r.key));
        }
    }
    let sidecar_bytes: Bytes = encode_sidecar(&mut new_entries)?;
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
            .ok_or(CompilerError::InvariantViolation {
                what: "rebalance: unknown binding",
            })?;
    let prior_binding =
        prior
            .bindings
            .iter()
            .find(|m| m.binding_id == *binding_id)
            .ok_or(CompilerError::InvariantViolation {
                what: "rebalance: missing prior binding metadata",
            })?;
    let combined_bbox =
        prior_binding
            .levels
            .first()
            .map(|l| l.combined_bbox)
            .ok_or(CompilerError::InvariantViolation {
                what: "rebalance: prior binding has no level metadata",
            })?;
    let sc = sidecar.ok_or(CompilerError::InvariantViolation {
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
            .ok_or(CompilerError::InvariantViolation {
                what: "rebalance: source page missing from prior manifest",
            })?
            .clone();
        source_pages.insert(k.clone(), entry);
    }

    // union of hilbert ranges; sidecar lookup gives us the feature id set.
    let union_ranges: Vec<(HilbertKey, HilbertKey)> = source_pages.values().map(|p| p.hilbert_range).collect();
    // bag semantics: dedup user_ids before the source fetch — a user_id
    // that appears N times in the multimap should still be fetched once,
    // since the source returns ALL its rows.
    let mut feature_ids = sc.user_ids_in_ranges(&union_ranges);
    feature_ids.sort_unstable();
    feature_ids.dedup();

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
    let stream = deps.source.fetch_by_feature_ids(&port_binding, &ids).await?;
    let mut rows = hydrate_keyed_rows(stream, combined_bbox).await?;
    rows.sort_by(|a, b| {
        a.key
            .cmp(&b.key)
            .then_with(|| a.feature.user_id.cmp(&b.feature.user_id))
            .then_with(|| a.row_fingerprint.cmp(&b.row_fingerprint))
    });

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
                    .ok_or(CompilerError::InvariantViolation {
                        what: "rebalance: split source page missing",
                    })?;
                let level_plan = binding_plan.levels.iter().find(|l| l.level == page.level).ok_or(
                    CompilerError::InvariantViolation {
                        what: "rebalance: split level plan missing",
                    },
                )?;
                let (lo, hi) = src.hilbert_range;
                // partition source-range rows into leveled (passes_min_size,
                // re-paged) and pruned (Independent label candidates only).
                // simplify is applied here so split-output pages match what
                // snapshot/rebuild would emit at this level.
                let mut in_range_leveled: Vec<KeyedRow> = Vec::new();
                let mut in_range_pruned: Vec<KeyedRow> = Vec::new();
                for r in rows.iter().filter(|r| r.key >= lo && r.key <= hi) {
                    if passes_min_size(&r.feature, level_plan.geometry_min_size_m) {
                        in_range_leveled.push(KeyedRow {
                            feature: FeatureGeom {
                                user_id: r.feature.user_id,
                                bbox: r.feature.bbox,
                                geom: simplify(&r.feature.geom, level_plan.vertex_tolerance_m, binding_plan.simplifier),
                            },
                            attrs: r.attrs.clone(),
                            geom_bytes_estimate: r.geom_bytes_estimate,
                            key: r.key,
                            row_fingerprint: r.row_fingerprint,
                        });
                    } else {
                        in_range_pruned.push(r.clone());
                    }
                }
                in_range_leveled.sort_by(|a, b| {
                    a.key
                        .cmp(&b.key)
                        .then_with(|| a.feature.user_id.cmp(&b.feature.user_id))
                        .then_with(|| a.row_fingerprint.cmp(&b.row_fingerprint))
                });
                in_range_pruned.sort_by_key(|r| r.key);
                drop_page_with_sidecars(&page, &layer_plans, outcome);
                if in_range_leveled.is_empty() || into == 0 {
                    // no rendered geometry survives -> drop the page; pruned
                    // labels have nowhere to live (matches incremental path).
                    continue;
                }
                let n = in_range_leveled.len();
                let into_usize = into as usize;
                let chunk = n.div_ceil(into_usize);
                let mut pruned_idx: usize = 0;
                let split_count = into_usize.min(n.div_ceil(chunk));
                for k in 0..into_usize {
                    let start = k * chunk;
                    if start >= n {
                        break;
                    }
                    let end = ((k + 1) * chunk).min(n);
                    let slice: Vec<KeyedRow> = in_range_leveled[start..end].to_vec();
                    if slice.is_empty() {
                        continue;
                    }
                    let new_page_id = bump_page_id(&mut next_page_id, page.level);
                    // last sub-page absorbs remaining pruned tail; earlier
                    // sub-pages take pruned rows up to their hilbert max.
                    let is_last = k + 1 == split_count;
                    let cap = if is_last {
                        HilbertKey::max()
                    } else {
                        slice.last().map(|x| x.key).unwrap_or(HilbertKey::min())
                    };
                    let pruned_slice = drain_pruned_through(&in_range_pruned, &mut pruned_idx, cap);
                    enforce_page_budget(
                        &slice,
                        working_set_bytes,
                        binding_plan.binding_id.as_str(),
                        PageId::new(new_page_id),
                    )?;
                    let entry = flush_page(deps, binding_plan, page.level, PageId::new(new_page_id), &slice).await?;
                    let mut class_acc = Vec::new();
                    let mut label_acc = Vec::new();
                    emit_layer_sidecars(
                        deps,
                        level_plan,
                        &entry,
                        &slice,
                        pruned_slice,
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
                    .ok_or(CompilerError::InvariantViolation {
                        what: "rebalance: merge left source missing",
                    })?;
                let src_r = source_pages
                    .get(&right)
                    .cloned()
                    .ok_or(CompilerError::InvariantViolation {
                        what: "rebalance: merge right source missing",
                    })?;
                let level_plan = binding_plan.levels.iter().find(|l| l.level == left.level).ok_or(
                    CompilerError::InvariantViolation {
                        what: "rebalance: merge level plan missing",
                    },
                )?;
                let (l_lo, l_hi) = src_l.hilbert_range;
                let (r_lo, r_hi) = src_r.hilbert_range;
                let mut merged_leveled: Vec<KeyedRow> = Vec::new();
                let mut merged_pruned: Vec<KeyedRow> = Vec::new();
                for r in rows
                    .iter()
                    .filter(|r| (r.key >= l_lo && r.key <= l_hi) || (r.key >= r_lo && r.key <= r_hi))
                {
                    if passes_min_size(&r.feature, level_plan.geometry_min_size_m) {
                        merged_leveled.push(KeyedRow {
                            feature: FeatureGeom {
                                user_id: r.feature.user_id,
                                bbox: r.feature.bbox,
                                geom: simplify(&r.feature.geom, level_plan.vertex_tolerance_m, binding_plan.simplifier),
                            },
                            attrs: r.attrs.clone(),
                            geom_bytes_estimate: r.geom_bytes_estimate,
                            key: r.key,
                            row_fingerprint: r.row_fingerprint,
                        });
                    } else {
                        merged_pruned.push(r.clone());
                    }
                }
                drop_page_with_sidecars(&left, &layer_plans, outcome);
                drop_page_with_sidecars(&right, &layer_plans, outcome);
                if merged_leveled.is_empty() {
                    continue;
                }
                let new_page_id = bump_page_id(&mut next_page_id, left.level);
                enforce_page_budget(
                    &merged_leveled,
                    working_set_bytes,
                    binding_plan.binding_id.as_str(),
                    PageId::new(new_page_id),
                )?;
                let entry = flush_page(
                    deps,
                    binding_plan,
                    left.level,
                    PageId::new(new_page_id),
                    &merged_leveled,
                )
                .await?;
                let mut class_acc = Vec::new();
                let mut label_acc = Vec::new();
                emit_layer_sidecars(
                    deps,
                    level_plan,
                    &entry,
                    &merged_leveled,
                    &merged_pruned,
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

/// Rebuild every page in `page_plan` against `binding_plan` by hydrating
/// rows through the supplied [`mars_source::CompileSession`] and emitting
/// page artifacts + class/label sidecars. Returns a [`BindingOutput`] in
/// the same shape `snapshot::snapshot_one_binding` produced, so the caller
/// can fold it into a manifest identically to the legacy bootstrap path.
///
/// The session must be freshly opened against `binding_plan`; the plan was
/// built from its pass-1 scan and pass-2 here re-uses the same snapshot
/// transaction.
pub(crate) async fn rebuild_binding_from_plan<'a>(
    deps: &Deps,
    plan: &BootstrapPlan,
    binding_plan: &crate::plan::BindingPlan,
    page_plan: &PagePlan,
    session: &mut (dyn mars_source::CompileSession + 'a),
    working_set_bytes: u64,
) -> Result<BindingOutput, CompilerError> {
    if page_plan.feature_count_total == 0 {
        return Ok(BindingOutput {
            meta: BindingMetadata {
                binding_id: binding_plan.binding_id.clone(),
                source_table: binding_plan.source_table.clone(),
                native_crs: binding_plan.native_crs.clone(),
                feature_count_total: 0,
                levels: binding_plan.levels.iter().map(empty_level_metadata).collect(),
                page_membership_sidecar: None,
            },
            pages: Vec::new(),
            class_sidecars: Vec::new(),
            label_sidecars: Vec::new(),
        });
    }
    if binding_plan.levels.len() != page_plan.levels.len() {
        return Err(CompilerError::InvariantViolation {
            what: "rebuild_from_plan: level count mismatch between binding and plan",
        });
    }

    // union of feature ids across every planned page in every level. one
    // fetch_by_feature_ids call per binding -> the postgres adapter chunks
    // 16 384 ids per SQL roundtrip internally.
    let mut union_ids: BTreeSet<i64> = BTreeSet::new();
    for level_pp in &page_plan.levels {
        for planned in &level_pp.pages {
            for id in &planned.feature_ids {
                union_ids.insert(*id);
            }
        }
    }
    let ids: Vec<i64> = union_ids.into_iter().collect();
    let stream = session.fetch_by_feature_ids(&ids).await?;
    let rows = hydrate_keyed_rows(stream, page_plan.combined_bbox).await?;

    let layer_plans: Vec<&LayerPlan> = plan.layers_for(&binding_plan.binding_id).collect();
    let mut all_pages: Vec<PageEntry> = Vec::new();
    let mut levels_meta: Vec<LevelMetadata> = Vec::with_capacity(binding_plan.levels.len());
    let mut class_sidecars: Vec<LayerSidecarEntry> = Vec::new();
    let mut label_sidecars: Vec<LayerSidecarEntry> = Vec::new();

    for (level_plan, level_pp) in binding_plan.levels.iter().zip(&page_plan.levels) {
        debug_assert_eq!(level_plan.level, level_pp.level);
        let mut level_pages: Vec<PageEntry> = Vec::new();
        for planned in &level_pp.pages {
            let (lo, hi) = planned.hilbert_range;
            let mut page_rows: Vec<KeyedRow> = Vec::new();
            let mut pruned_rows: Vec<KeyedRow> = Vec::new();
            for r in rows.iter().filter(|r| r.key >= lo && r.key <= hi) {
                if crate::decimate::passes_min_size(&r.feature, level_plan.geometry_min_size_m) {
                    page_rows.push(KeyedRow {
                        feature: FeatureGeom {
                            user_id: r.feature.user_id,
                            bbox: r.feature.bbox,
                            geom: crate::decimate::simplify(
                                &r.feature.geom,
                                level_plan.vertex_tolerance_m,
                                binding_plan.simplifier,
                            ),
                        },
                        attrs: r.attrs.clone(),
                        geom_bytes_estimate: r.geom_bytes_estimate,
                        key: r.key,
                        row_fingerprint: r.row_fingerprint,
                    });
                } else {
                    pruned_rows.push(r.clone());
                }
            }
            page_rows.sort_by(|a, b| {
                a.key
                    .cmp(&b.key)
                    .then_with(|| a.feature.user_id.cmp(&b.feature.user_id))
                    .then_with(|| a.row_fingerprint.cmp(&b.row_fingerprint))
            });
            enforce_page_budget(
                &page_rows,
                working_set_bytes,
                binding_plan.binding_id.as_str(),
                planned.page_id,
            )?;
            if page_rows.is_empty() {
                // pruned-only page: drop entirely, matches incremental contract.
                continue;
            }
            let entry = flush_page(deps, binding_plan, level_plan.level, planned.page_id, &page_rows).await?;
            let mut class_acc: Vec<LayerSidecarEntry> = Vec::new();
            let mut label_acc: Vec<LayerSidecarEntry> = Vec::new();
            emit_layer_sidecars(
                deps,
                level_plan,
                &entry,
                &page_rows,
                &pruned_rows,
                &layer_plans,
                &mut class_acc,
                &mut label_acc,
            )
            .await?;
            level_pages.push(entry);
            class_sidecars.append(&mut class_acc);
            label_sidecars.append(&mut label_acc);
        }
        levels_meta.push(LevelMetadata {
            level: level_plan.level,
            vertex_tolerance_m: level_plan.vertex_tolerance_m,
            geometry_min_size_m: level_plan.geometry_min_size_m,
            label_min_priority: level_plan.label_min_priority,
            page_count: level_pages.len() as u32,
            combined_bbox: page_plan.combined_bbox,
            hilbert_range_table: level_pages.iter().map(|p| p.hilbert_range).collect(),
        });
        all_pages.append(&mut level_pages);
    }

    // page-membership sidecar: pass-1 already collected (user_id, hilbert_key)
    // for every unfiltered row, so we just hand the slice to encode_sidecar.
    let mut sidecar_entries = page_plan.sidecar_entries.clone();
    let sidecar_bytes = encode_sidecar(&mut sidecar_entries)?;
    let sidecar_hash = compute_content_hash(&sidecar_bytes);
    let sidecar_key = membership_sidecar_object_key(binding_plan.binding_id.as_str(), &sidecar_hash)?;
    let sidecar_size = sidecar_bytes.len() as u64;
    deps.store.put(&sidecar_key, sidecar_bytes).await?;

    let meta = BindingMetadata {
        binding_id: binding_plan.binding_id.clone(),
        source_table: binding_plan.source_table.clone(),
        native_crs: binding_plan.native_crs.clone(),
        feature_count_total: page_plan.feature_count_total,
        levels: levels_meta,
        page_membership_sidecar: Some(ArtifactEntry {
            key: sidecar_key,
            hash: sidecar_hash,
            size_bytes: sidecar_size,
        }),
    };
    Ok(BindingOutput {
        meta,
        pages: all_pages,
        class_sidecars,
        label_sidecars,
    })
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

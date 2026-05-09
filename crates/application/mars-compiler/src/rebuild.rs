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
use mars_artifact::{
    ArtifactKind, ArtifactWriter, AttrValue as ArtAttrValue, FeatureGeom, LabelCandidate, MAX_ROW_BYTES,
    SpatialIndexBuilder, compute_content_hash, encode_row, wkb_to_feature_geom,
};
use mars_source::{AttrValue, RowBytes, SourceBinding as PortBinding, SourceCollectionId, SourceError};
use mars_types::{
    ArtifactEntry, ArtifactKey, Bbox, BindingId, BindingMetadata, ContentHash, DecimationLevel, HilbertKey,
    LayerSidecarEntry, LayerSidecarKind, LevelMetadata, Manifest, PageEntry, PageId, PageKey,
};

use crate::class_eval::{LabelSpec, RowAttrs, assign_class, emit_label_candidate};
use crate::decimate::{passes_min_size, simplify};
use crate::external_sort::WorkingSetGuard;
use crate::incremental::{BindingDirty, DirtyPages};
use crate::page_plan::{PagePlan, compute_page_plan};
use crate::plan::{BindingPlan, BootstrapPlan, LayerPlan, LevelPlan};
use crate::rebalance::RebalanceOp;
use crate::sidecar::{SidecarReader, encode_sidecar};
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
) -> Result<Vec<KeyedRow>, CompilerError> {
    let mut rows: Vec<KeyedRow> = Vec::new();
    while let Some(item) = stream.next().await {
        let row: RowBytes = item?;
        let geom_bytes_estimate = row.geometry.len() as u64;
        let row_fingerprint = compute_row_fingerprint_for_row(&row);
        let feature = wkb_to_feature_geom(&row.geometry, row.feature_id)?;
        let cx = (f64::from(feature.bbox[0]) + f64::from(feature.bbox[2])) / 2.0;
        let cy = (f64::from(feature.bbox[1]) + f64::from(feature.bbox[3])) / 2.0;
        let key = crate::hilbert::key_from_centroid(cx, cy, combined_bbox);
        rows.push(KeyedRow {
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
    rows: &[KeyedRow],
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
///
/// `working_set_bytes` is the per-page hydrated-row ceiling (pass-2);
/// `plan_budget_bytes` caps the pass-1 page-planner allocation when the
/// truncate path runs the unified compile pipeline against a binding.
pub async fn rebuild_pages(
    deps: &Deps,
    plan: &BootstrapPlan,
    prior: &Manifest,
    sidecars: &HashMap<BindingId, SidecarReader<'_>>,
    dirty: DirtyPages,
    working_set_bytes: u64,
    plan_budget_bytes: u64,
) -> Result<RebuildOutcome, CompilerError> {
    let mut outcome = RebuildOutcome::default();
    for (binding_id, binding_dirty) in dirty.per_binding {
        if binding_dirty.truncated {
            rebuild_binding_truncate(
                deps,
                plan,
                prior,
                &binding_id,
                working_set_bytes,
                plan_budget_bytes,
                &mut outcome,
            )
            .await?;
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

/// Truncate-class rebuild: re-derive the binding's pages from scratch via
/// the unified compile pipeline. Drops every prior page + sidecar of the
/// binding so the new plan replaces them cleanly even when the new page
/// count differs from the old one.
async fn rebuild_binding_truncate(
    deps: &Deps,
    plan: &BootstrapPlan,
    prior: &Manifest,
    binding_id: &BindingId,
    working_set_bytes: u64,
    plan_budget_bytes: u64,
    outcome: &mut RebuildOutcome,
) -> Result<(), CompilerError> {
    let binding_plan =
        plan.bindings
            .iter()
            .find(|b| b.binding_id == *binding_id)
            .ok_or(CompilerError::InvariantViolation {
                what: "rebuild: unknown binding for truncate",
            })?;

    // drop every prior artifact under this binding; the unified pipeline
    // emits a fresh page set with page_ids restarting at 0, so any prior
    // page_id higher than the new count would orphan otherwise.
    for prior_page in &prior.pages {
        if prior_page.key.binding_id == *binding_id {
            outcome.dropped_pages.push(prior_page.key.clone());
        }
    }
    for sc in &prior.class_sidecars {
        if sc.page_key.binding_id == *binding_id {
            outcome
                .dropped_class_sidecars
                .push((sc.layer_id.clone(), sc.page_key.clone()));
        }
    }
    for sc in &prior.label_sidecars {
        if sc.page_key.binding_id == *binding_id {
            outcome
                .dropped_label_sidecars
                .push((sc.layer_id.clone(), sc.page_key.clone()));
        }
    }

    let port_binding = PortBinding::new(
        SourceCollectionId::new(binding_plan.binding_id.as_str()),
        binding_schema(&binding_plan.source_table),
        binding_table(&binding_plan.source_table),
        binding_plan.geometry_column.clone(),
        binding_plan.id_column.as_deref().unwrap_or("id"),
        binding_plan.attributes.clone(),
        binding_plan.native_crs.clone(),
    )?;
    let mut session = deps.source.open_compile_session(&port_binding).await?;
    let page_plan = compute_page_plan(session.as_mut(), binding_plan, plan_budget_bytes).await?;
    let mut out = rebuild_binding_from_plan(
        deps,
        plan,
        binding_plan,
        &page_plan,
        session.as_mut(),
        working_set_bytes,
    )
    .await?;
    session.close().await?;

    outcome.refreshed_bindings.push(out.meta);
    outcome.replacement_pages.append(&mut out.pages);
    outcome.replacement_class_sidecars.append(&mut out.class_sidecars);
    outcome.replacement_label_sidecars.append(&mut out.label_sidecars);
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
pub async fn rebuild_binding_from_plan<'a>(
    deps: &Deps,
    plan: &BootstrapPlan,
    binding_plan: &BindingPlan,
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
    if sidecar_size > binding_plan.sidecar_size_warn_bytes {
        tracing::warn!(
            binding = binding_plan.binding_id.as_str(),
            size_bytes = sidecar_size,
            threshold_bytes = binding_plan.sidecar_size_warn_bytes,
            "page-membership sidecar exceeds warning threshold; consider REPLICA IDENTITY FULL for this binding (LAZARUS bailout 4)"
        );
        deps.metrics
            .inc_compiler_sidecar_threshold_warning(binding_plan.binding_id.as_str());
    }
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

// -- per-page render helpers --------------------------------------------
//
// shared by the unified compile pipeline (truncate + bootstrap-from-plan
// via `rebuild_binding_from_plan`), the incremental rebuild path, and the
// rebalance executor. one source row decoded into a feature, with attrs
// preserved for class / label evaluation and a hilbert key over the
// binding's combined bbox.
//
// `row_fingerprint` is BLAKE3 over the row, used as the final tiebreaker
// after `(hilbert_key, user_id)` so rows with the same hilbert key and the
// same user_id (a source row exploded into multiple parts) sort
// deterministically across compile passes.
#[derive(Debug, Clone)]
pub(crate) struct KeyedRow {
    pub(crate) feature: FeatureGeom,
    pub(crate) attrs: Arc<Vec<(String, AttrValue)>>,
    pub(crate) geom_bytes_estimate: u64,
    pub(crate) key: HilbertKey,
    pub(crate) row_fingerprint: u64,
}

/// Output of one binding compile through the unified pipeline.
#[derive(Debug)]
pub struct BindingOutput {
    pub meta: BindingMetadata,
    pub pages: Vec<PageEntry>,
    pub class_sidecars: Vec<LayerSidecarEntry>,
    pub label_sidecars: Vec<LayerSidecarEntry>,
}

/// Stable per-row tiebreaker. BLAKE3 over `(geometry_bytes ||
/// canonicalised attribute bytes)` truncated to u64.
pub(crate) fn compute_row_fingerprint_for_row(row: &RowBytes) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&row.geometry);
    // canonicalise: sort attribute pairs by name so reordering doesn't shift
    // the fingerprint.
    let mut sorted: Vec<&(String, AttrValue)> = row.attributes.iter().collect();
    sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    for (name, value) in sorted {
        hasher.update(name.as_bytes());
        hasher.update(b"\0");
        hash_attr_value(&mut hasher, value);
        hasher.update(b"\0");
    }
    let mut out = [0u8; 8];
    hasher.finalize_xof().fill(&mut out);
    u64::from_le_bytes(out)
}

fn hash_attr_value(hasher: &mut blake3::Hasher, v: &AttrValue) {
    match v {
        AttrValue::Null => hasher.update(b"N"),
        AttrValue::Bool(b) => hasher.update(if *b { b"BT" } else { b"BF" }),
        AttrValue::Int(i) => {
            hasher.update(b"I");
            hasher.update(&i.to_le_bytes())
        }
        AttrValue::Float(f) => {
            hasher.update(b"F");
            hasher.update(&f.to_bits().to_le_bytes())
        }
        AttrValue::String(s) => {
            hasher.update(b"S");
            hasher.update(s.as_bytes())
        }
    };
}

/// Pull pruned rows whose hilbert key is `<= cap` off the head of the
/// pre-sorted slice, advancing `idx`.
pub(crate) fn drain_pruned_through<'a>(pruned: &'a [KeyedRow], idx: &mut usize, cap: HilbertKey) -> &'a [KeyedRow] {
    let start = *idx;
    while *idx < pruned.len() && pruned[*idx].key <= cap {
        *idx += 1;
    }
    &pruned[start..*idx]
}

/// Encode rows into a page artifact, write it to the object store, and
/// return the matching [`PageEntry`]. Rows arrive in deterministic slot
/// order; position becomes the substrate primary key.
pub(crate) async fn flush_page(
    deps: &Deps,
    binding: &BindingPlan,
    level: DecimationLevel,
    page_id: PageId,
    rows: &[KeyedRow],
) -> Result<PageEntry, CompilerError> {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;

    let mut spatial_index = SpatialIndexBuilder::new(mars_artifact::DEFAULT_NODE_SIZE)?;
    let mut features: Vec<FeatureGeom> = Vec::with_capacity(rows.len());
    let mut attrs_pairs: Vec<(u32, Vec<u8>)> = Vec::with_capacity(rows.len());

    for (slot, r) in rows.iter().enumerate() {
        let bb = r.feature.bbox;
        let slot_u32 = u32::try_from(slot).map_err(|_| CompilerError::InvariantViolation {
            what: "page slot overflow",
        })?;
        spatial_index.add(slot_u32, bb);
        if (bb[0] as f64) < min_x {
            min_x = bb[0] as f64;
        }
        if (bb[1] as f64) < min_y {
            min_y = bb[1] as f64;
        }
        if (bb[2] as f64) > max_x {
            max_x = bb[2] as f64;
        }
        if (bb[3] as f64) > max_y {
            max_y = bb[3] as f64;
        }
        features.push(r.feature.clone());
        let pairs: Vec<(String, ArtAttrValue)> = r
            .attrs
            .iter()
            .map(|(k, v)| (k.clone(), attr_value_to_artifact(v)))
            .collect();
        let row_bytes = encode_row(&pairs)?;
        if row_bytes.len() > MAX_ROW_BYTES {
            return Err(CompilerError::RowAttributesTooLarge {
                feature_id: r.feature.user_id,
                bytes: row_bytes.len(),
                max: MAX_ROW_BYTES,
            });
        }
        attrs_pairs.push((slot_u32, row_bytes.to_vec()));
    }

    let page_bbox = Bbox::new(min_x, min_y, max_x, max_y);
    let spatial_index_bytes = spatial_index.finish()?;

    let mut writer = ArtifactWriter::new(ArtifactKind::Source);
    writer
        .add_spatial_index(spatial_index_bytes)
        .add_geometry_payload(features)
        .add_attributes(attrs_pairs)
        .set_bbox(page_bbox)
        .set_feature_count(rows.len() as u64);
    let artifact_bytes: Bytes = writer.finish()?;
    let hash = compute_content_hash(&artifact_bytes);

    let page_key = PageKey {
        binding_id: binding.binding_id.clone(),
        level,
        page_id,
    };
    let object_key = page_key.object_key(&hash)?;
    let size_bytes = artifact_bytes.len() as u64;
    deps.store.put(&object_key, artifact_bytes).await?;

    let hilbert_lo = rows.iter().map(|r| r.key).min().unwrap_or(HilbertKey::min());
    let hilbert_hi = rows.iter().map(|r| r.key).max().unwrap_or(HilbertKey::max());

    Ok(PageEntry {
        key: page_key,
        content_hash: hash,
        spatial_bbox: page_bbox,
        hilbert_range: (hilbert_lo, hilbert_hi),
        feature_count: rows.len() as u64,
        size_bytes,
    })
}

/// For each layer plan: evaluate class assignments against `rows`, emit a
/// label candidate per row whose attrs match, and write per-layer class /
/// label sidecars to the store.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn emit_layer_sidecars(
    deps: &Deps,
    level: &LevelPlan,
    page: &PageEntry,
    rows: &[KeyedRow],
    pruned: &[KeyedRow],
    layers: &[&LayerPlan],
    out_class: &mut Vec<LayerSidecarEntry>,
    out_label: &mut Vec<LayerSidecarEntry>,
) -> Result<(), CompilerError> {
    for layer in layers {
        let mut assignments: Vec<(u32, u16)> = Vec::with_capacity(rows.len());
        let mut labels: Vec<LabelCandidate> = Vec::new();

        let when_clauses: Vec<Option<mars_expr::Expr>> = layer.classes.iter().map(|c| c.when.clone()).collect();
        let style_refs: Vec<String> = layer.classes.iter().map(|c| c.style_ref.clone()).collect();
        let label_spec = layer.label.as_ref().map(|l| LabelSpec {
            priority: l.style.priority,
            text: &l.text,
            placement: &l.placement,
            style_ref_idx: u16::try_from(style_refs.len()).unwrap_or(u16::MAX),
        });

        for (slot, r) in rows.iter().enumerate() {
            let slot_u32 = u32::try_from(slot).map_err(|_| CompilerError::InvariantViolation {
                what: "page slot overflow",
            })?;
            let attrs = RowAttrs::new(r.attrs.as_ref());
            if let Some(idx) = assign_class(&when_clauses, &attrs) {
                assignments.push((slot_u32, idx));
            }
            if let Some(spec) = &label_spec
                && let Some(c) = emit_label_candidate(
                    &r.feature,
                    Some(slot_u32),
                    &attrs,
                    spec,
                    layer.label_survival,
                    level.label_min_priority,
                )
            {
                labels.push(c);
            }
        }

        if let Some(spec) = &label_spec {
            for r in pruned {
                let attrs = RowAttrs::new(r.attrs.as_ref());
                if let Some(c) = emit_label_candidate(
                    &r.feature,
                    None,
                    &attrs,
                    spec,
                    layer.label_survival,
                    level.label_min_priority,
                ) {
                    labels.push(c);
                }
            }
        }

        let mut style_refs_full = style_refs;
        if let Some(label_plan) = layer.label.as_ref() {
            style_refs_full.push(label_plan.style_ref.clone());
        }

        let class_bytes = build_class_artifact(&assignments, &style_refs_full, page.spatial_bbox)?;
        let class_hash = compute_content_hash(&class_bytes);
        let class_size = class_bytes.len() as u64;
        let class_entry = LayerSidecarEntry {
            layer_id: layer.layer_id.clone(),
            page_key: page.key.clone(),
            content_hash: class_hash,
            size_bytes: class_size,
            kind: LayerSidecarKind::Class,
        };
        let class_obj = class_entry.object_key()?;
        deps.store.put(&class_obj, class_bytes).await?;
        out_class.push(class_entry);

        if !labels.is_empty() {
            // slotted entries first (ascending feature_idx), pruned at the tail.
            labels.sort_by_key(|c| (c.feature_idx.is_none(), c.feature_idx.unwrap_or(0)));
            let label_bytes = build_label_artifact(&labels, page.spatial_bbox)?;
            let label_hash = compute_content_hash(&label_bytes);
            let label_size = label_bytes.len() as u64;
            let label_entry = LayerSidecarEntry {
                layer_id: layer.layer_id.clone(),
                page_key: page.key.clone(),
                content_hash: label_hash,
                size_bytes: label_size,
                kind: LayerSidecarKind::Label,
            };
            let label_obj = label_entry.object_key()?;
            deps.store.put(&label_obj, label_bytes).await?;
            out_label.push(label_entry);
        }
    }
    Ok(())
}

fn build_class_artifact(
    assignments: &[(u32, u16)],
    style_refs: &[String],
    page_bbox: Bbox,
) -> Result<Bytes, CompilerError> {
    let mut writer = ArtifactWriter::new(ArtifactKind::Layer);
    writer
        .add_class_assignment(assignments)
        .add_style_refs(style_refs)
        .set_bbox(page_bbox)
        .set_feature_count(assignments.len() as u64);
    writer.finish().map_err(CompilerError::from)
}

fn build_label_artifact(labels: &[LabelCandidate], page_bbox: Bbox) -> Result<Bytes, CompilerError> {
    let mut writer = ArtifactWriter::new(ArtifactKind::Layer);
    writer
        .add_label_candidates(labels)
        .set_bbox(page_bbox)
        .set_feature_count(labels.len() as u64);
    writer.finish().map_err(CompilerError::from)
}

pub(crate) fn empty_level_metadata(level: &LevelPlan) -> LevelMetadata {
    LevelMetadata {
        level: level.level,
        vertex_tolerance_m: level.vertex_tolerance_m,
        geometry_min_size_m: level.geometry_min_size_m,
        label_min_priority: level.label_min_priority,
        page_count: 0,
        combined_bbox: Bbox::new(0.0, 0.0, 0.0, 0.0),
        hilbert_range_table: Vec::new(),
    }
}

pub(crate) fn binding_schema(from: &str) -> &str {
    from.split_once('.').map(|(s, _)| s).unwrap_or("public")
}

pub(crate) fn binding_table(from: &str) -> &str {
    from.split_once('.').map(|(_, t)| t).unwrap_or(from)
}

pub(crate) fn membership_sidecar_object_key(binding: &str, hash: &ContentHash) -> Result<ArtifactKey, CompilerError> {
    if binding.contains('/') || binding.contains('\0') {
        return Err(CompilerError::InvalidBindingId {
            binding: binding.to_string(),
        });
    }
    Ok(ArtifactKey::new(format!(
        "bnd/{binding}/sidecar/{hex}.pmsc",
        hex = hash.to_hex()
    )))
}

fn attr_value_to_artifact(v: &AttrValue) -> ArtAttrValue {
    match v {
        AttrValue::Null => ArtAttrValue::Null,
        AttrValue::Bool(b) => ArtAttrValue::Bool(*b),
        AttrValue::Int(i) => ArtAttrValue::Int(*i),
        AttrValue::Float(f) => ArtAttrValue::Float(*f),
        AttrValue::String(s) => ArtAttrValue::String(s.clone()),
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

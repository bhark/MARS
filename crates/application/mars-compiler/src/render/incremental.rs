//! Per-binding dirty-set rebuild path.
//!
//! [`rebuild_pages`] is the cycle entry point: it dispatches each dirty
//! binding to either [`rebuild_binding_truncate`] (delegates to pass-2 via
//! [`super::rebuild_binding_from_plan`]) or [`rebuild_binding_incremental`]
//! (the incremental cycle: assemble dirty hilbert ranges → resolve
//! affected feature ids via the page-membership sidecar → re-fetch from
//! source → partition into dirty pages → re-decimate, emit page +
//! sidecars, refresh the per-binding sidecar).
//!
//! Per-binding parallelism is bounded by `binding_parallelism`; memory and
//! disk governors admit across concurrent bindings so the budgets stay
//! enforced. Each binding writes into a local [`RebuildOutcome`] that is
//! only merged into the shared one on success, so isolating policy can
//! drop partial work cleanly.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use bytes::Bytes;
use futures_util::StreamExt;
use mars_artifact::{FeatureGeom, compute_content_hash};
use mars_source::{SourceBinding as PortBinding, SourceCollectionId};
use mars_types::{ArtifactEntry, BindingId, DecimationLevel, HilbertKey, LayerSidecarEntry, Manifest, PageId};

use crate::disk_governor::DiskGovernor;
use crate::incremental::{BindingDirty, DirtyPages};
use crate::memory_governor::MemoryGovernor;
use crate::page_plan::compute_page_plan;
use crate::plan::{BootstrapPlan, LayerPlan, LevelPlan};
use crate::sidecar::{SidecarReader, encode_sidecar};
use crate::{CompilerError, Deps};

use super::flush::{emit_layer_sidecars, filter_unmatched_rows, flush_page};
use super::{
    KeyedRow, RebuildOutcome, enforce_page_budget, hydrate_keyed_rows, membership_sidecar_object_key,
    rebuild_binding_from_plan,
};

/// Run one rebuild pass for the given dirty set. Per-binding sidecar
/// thresholds are read from the matching [`crate::plan::BindingPlan`].
///
/// `working_set_bytes` is the per-page hydrated-row ceiling (pass-2);
/// `plan_budget_bytes` caps the pass-1 page-planner allocation when the
/// truncate path runs the unified compile pipeline against a binding.
///
/// `binding_parallelism` bounds the number of bindings rebuilt
/// concurrently within this pass. `1` reproduces the prior sequential
/// behaviour; higher values turn thundering-herd events
/// (schema flips, large reconciliations) into max-of-binding wall-clock
/// instead of sum-of-bindings. Memory and disk governors admit across
/// concurrent bindings, so the budgets stay enforced.
///
/// Each binding's rebuild writes into a local [`RebuildOutcome`] which is
/// only merged into the shared outcome on success. Under
/// [`mars_config::BindingFailurePolicy::Isolate`] a per-binding error is
/// logged, metered, and discarded so the cycle still publishes the other
/// bindings' progress; under
/// [`mars_config::BindingFailurePolicy::FailCycle`] the first error
/// aborts the pass (under concurrent execution, "first" is whichever
/// in-flight rebuild errors first - non-deterministic across runs).
#[allow(clippy::too_many_arguments)]
pub async fn rebuild_pages(
    deps: &Deps,
    plan: &BootstrapPlan,
    prior: &Manifest,
    sidecars: &HashMap<BindingId, SidecarReader<'_>>,
    dirty: DirtyPages,
    working_set_bytes: u64,
    plan_budget_bytes: u64,
    in_flight_budget_bytes: u64,
    spill_dir: &std::path::Path,
    spill_open_file_limit: usize,
    governor: &MemoryGovernor,
    disk_governor: &DiskGovernor,
    failure_policy: mars_config::BindingFailurePolicy,
    binding_parallelism: usize,
) -> Result<RebuildOutcome, CompilerError> {
    use mars_observability::binding_rebuild_kind;

    let DirtyPages {
        per_binding,
        warnings: _,
        failed,
    } = dirty;

    let mut outcome = RebuildOutcome::default();
    // bindings the source flagged as degraded this cycle: skip the
    // rebuild dispatch so prior pages keep being served, and surface a
    // bounded healthcheck metric so operators see the gap.
    for (binding_id, reason) in &failed {
        deps.metrics.inc_compiler_binding_rebuild_failure(
            binding_id.as_str(),
            mars_observability::binding_rebuild_failure_reason::BINDING_UNHEALTHY,
        );
        tracing::warn!(
            binding = binding_id.as_str(),
            reason = %reason,
            "binding degraded by source; skipping rebuild, prior pages preserved"
        );
    }

    let parallelism = binding_parallelism.max(1);
    let mut work_iter = per_binding
        .into_iter()
        .filter(|(binding_id, _)| !failed.contains_key(binding_id));
    let mut pending = futures_util::stream::FuturesUnordered::new();
    loop {
        while pending.len() < parallelism
            && let Some((binding_id, binding_dirty)) = work_iter.next()
        {
            pending.push(async move {
                let started = std::time::Instant::now();
                let mut local = RebuildOutcome::default();
                let (kind, res) = if binding_dirty.truncated {
                    let res = rebuild_binding_truncate(
                        deps,
                        plan,
                        prior,
                        &binding_id,
                        working_set_bytes,
                        plan_budget_bytes,
                        in_flight_budget_bytes,
                        spill_dir,
                        spill_open_file_limit,
                        governor,
                        disk_governor,
                        &mut local,
                    )
                    .await;
                    (binding_rebuild_kind::TRUNCATE, res)
                } else {
                    let sidecar_warn = plan
                        .bindings
                        .iter()
                        .find(|b| b.binding_id == binding_id)
                        .map(|b| b.sidecar_size_warn_bytes)
                        .unwrap_or(u64::MAX);
                    let res = rebuild_binding_incremental(
                        deps,
                        plan,
                        prior,
                        sidecars.get(&binding_id),
                        &binding_id,
                        &binding_dirty,
                        working_set_bytes,
                        sidecar_warn,
                        &mut local,
                    )
                    .await;
                    (binding_rebuild_kind::INCREMENTAL, res)
                };
                deps.metrics
                    .observe_compiler_binding_rebuild_duration(kind, started.elapsed());
                (binding_id, local, res)
            });
        }
        match pending.next().await {
            Some((_, local, Ok(()))) => outcome.absorb(local),
            Some((binding_id, _local, Err(err))) => match failure_policy {
                mars_config::BindingFailurePolicy::FailCycle => return Err(err),
                mars_config::BindingFailurePolicy::Isolate => {
                    let reason = classify_compiler_error(&err);
                    deps.metrics
                        .inc_compiler_binding_rebuild_failure(binding_id.as_str(), reason);
                    tracing::error!(
                        binding = binding_id.as_str(),
                        reason,
                        error = %err,
                        "binding rebuild failed; isolating - prior pages preserved in published manifest"
                    );
                    // drop `local`: its partial writes never reach the outer outcome.
                }
            },
            None => break,
        }
    }
    Ok(outcome)
}

/// Classify a [`CompilerError`] into one of the bounded reason labels for
/// the `mars_compiler_binding_rebuild_failures_total` counter. Keeps
/// metric label cardinality flat regardless of the underlying error
/// message.
fn classify_compiler_error(err: &CompilerError) -> &'static str {
    use mars_observability::binding_rebuild_failure_reason as r;
    match err {
        CompilerError::Source(_) => r::SOURCE,
        CompilerError::Store(_) => r::STORE,
        CompilerError::Artifact(_)
        | CompilerError::Sidecar(_)
        | CompilerError::Wkb(_)
        | CompilerError::Attr(_)
        | CompilerError::Plan(_)
        | CompilerError::ScratchBudgetExceeded { .. }
        | CompilerError::BootstrapPlanTooLarge { .. }
        | CompilerError::RowAttributesTooLarge { .. } => r::COMPILE,
        _ => r::OTHER,
    }
}

/// Truncate-class rebuild: re-derive the binding's pages from scratch via
/// the unified compile pipeline. Drops every prior page + sidecar of the
/// binding so the new plan replaces them cleanly even when the new page
/// count differs from the old one.
#[allow(clippy::too_many_arguments)]
async fn rebuild_binding_truncate(
    deps: &Deps,
    plan: &BootstrapPlan,
    prior: &Manifest,
    binding_id: &BindingId,
    working_set_bytes: u64,
    plan_budget_bytes: u64,
    in_flight_budget_bytes: u64,
    spill_dir: &std::path::Path,
    spill_open_file_limit: usize,
    governor: &MemoryGovernor,
    disk_governor: &DiskGovernor,
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
        binding_plan.source_table.clone(),
        binding_plan.geometry_field.clone(),
        binding_plan.id_field.as_deref().unwrap_or("id"),
        binding_plan.attributes.clone(),
        binding_plan.native_crs.clone(),
    )?
    .with_filter(binding_plan.filter.clone())
    .with_dsn(binding_plan.dsn.clone());
    let source = deps.source_for(binding_plan)?;
    let mut session = source.open_compile_session(&port_binding).await?;
    let work = async {
        let page_plan = compute_page_plan(session.as_mut(), binding_plan, plan_budget_bytes, spill_dir).await?;
        rebuild_binding_from_plan(
            deps,
            plan,
            binding_plan,
            &page_plan,
            session.as_mut(),
            working_set_bytes,
            in_flight_budget_bytes,
            spill_dir,
            spill_open_file_limit,
            governor,
            disk_governor,
        )
        .await
    }
    .await;
    let mut out = match work {
        Ok(out) => {
            session.commit().await?;
            out
        }
        Err(err) => {
            if let Err(rb) = session.rollback().await {
                tracing::warn!(error = %rb, "compile session rollback failed");
            }
            return Err(err);
        }
    };

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
    let combined_bbox = prior_binding.combined_bbox;

    // 1. assemble the union of dirty hilbert ranges across all dirty levels.
    //    look up by `page_id` in the level's hilbert_range_table; the table
    //    is keyed by PageId, not by table-position (rebalance allocates
    //    fresh ids that no longer match position).
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
            if let Some((lo, hi, _)) = level_meta
                .hilbert_range_table
                .iter()
                .find(|(_, _, id)| id == page_id)
                .copied()
            {
                let range = (lo, hi);
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
        binding_plan.source_table.clone(),
        binding_plan.geometry_field.clone(),
        binding_plan.id_field.as_deref().unwrap_or("id"),
        binding_plan.attributes.clone(),
        binding_plan.native_crs.clone(),
    )?
    .with_filter(binding_plan.filter.clone())
    .with_dsn(binding_plan.dsn.clone());
    let ids: Vec<i64> = feature_ids
        .iter()
        .map(|f| i64::try_from(*f).unwrap_or(i64::MAX))
        .collect();
    let source = deps.source_for(binding_plan)?;
    let stream = source.stream_rows_by_id(&port_binding, &ids).await?;
    let rows = hydrate_keyed_rows(stream, combined_bbox).await?;
    let mut returned_counts: BTreeMap<u64, u32> = BTreeMap::new();
    for r in &rows {
        *returned_counts.entry(r.feature.user_id).or_default() += 1;
    }

    // 3b. partition fetched rows into dirty pages per level. each row is
    //     attributed to exactly one dirty page using the same placement rule
    //     the rest of the system trusts. boundary duplicates are resolved by
    //     lowest PageId so a key lands in exactly one page.
    let mut partitioned: BTreeMap<(DecimationLevel, PageId), Vec<KeyedRow>> = BTreeMap::new();
    for (level, dirty_pages) in &dirty_pages_by_level {
        let level_meta =
            prior_binding
                .levels
                .iter()
                .find(|m| m.level == *level)
                .ok_or(CompilerError::InvariantViolation {
                    what: "rebuild: missing prior level metadata for partition",
                })?;
        let dirty_set: BTreeSet<PageId> = dirty_pages.iter().map(|(pid, _)| *pid).collect();
        for r in &rows {
            let mut candidates = crate::incremental::pages_for_key(level_meta, r.key);
            candidates.retain(|pid| dirty_set.contains(pid));
            if candidates.is_empty() {
                continue;
            }
            // deterministic tie-breaker: lowest PageId.
            candidates.sort_unstable();
            let page_id = candidates[0];
            partitioned.entry((*level, page_id)).or_default().push(r.clone());
        }
    }

    // 4. for every dirty page: use the pre-partitioned rows, re-decimate
    //    per the level rules, emit page + sidecars. empty pages are dropped.
    let layers: Vec<&LevelPlan> = binding_plan.levels.iter().collect();
    let layer_plans: Vec<&LayerPlan> = plan.layers_for(&binding_plan.binding_id).collect();
    for level_plan in &layers {
        let Some(dirty_pages) = dirty_pages_by_level.get(&level_plan.level) else {
            continue;
        };
        for (page_id, _) in dirty_pages {
            let mut page_rows: Vec<KeyedRow> = Vec::new();
            let mut pruned_rows: Vec<KeyedRow> = Vec::new();
            let page_source = partitioned.remove(&(level_plan.level, *page_id)).unwrap_or_default();
            for r in page_source {
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
                        attrs: r.attrs,
                        geom_bytes_estimate: r.geom_bytes_estimate,
                        key: r.key,
                        row_fingerprint: r.row_fingerprint,
                    });
                } else {
                    pruned_rows.push(r);
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
            // β.2: drop rows no layer's class chain matches before geometry
            // emit. metric is per-binding; per-layer bookkeeping is muddied
            // by shared bindings, binding is the right granularity.
            let (page_rows, dropped_unmatched) = filter_unmatched_rows(page_rows, &layer_plans);
            if dropped_unmatched > 0 {
                deps.metrics
                    .inc_compiler_features_unmatched(binding_plan.binding_id.as_str(), dropped_unmatched);
            }
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
            "page-membership sidecar exceeds warning threshold; consider REPLICA IDENTITY FULL for this binding"
        );
        deps.metrics
            .inc_compiler_sidecar_threshold_warning(binding_plan.binding_id.as_str());
    }
    let sidecar_hash = compute_content_hash(&sidecar_bytes);
    let sidecar_key = membership_sidecar_object_key(binding_plan.binding_id.as_str(), &sidecar_hash)?;
    deps.store.put(&sidecar_key, sidecar_bytes).await?;

    // 6. compute refreshed level metadata. for now keep prior level
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

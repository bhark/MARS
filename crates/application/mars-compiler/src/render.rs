//! Page emission and rebalance executor for the unified compile pipeline.
//!
//! This module is pass 2 of the unified compile flow. Bootstrap drives it
//! via [`rebuild_binding_from_plan`] which streams the bound collection once
//! per binding through [`mars_source::CompileSession::stream_rows`] and
//! buckets rows into the planned (level, page_id) targets keyed on
//! [`mars_source::SourceRowKey`]; completed pages eager-flush, simplify,
//! emit artifacts, and write class / label sidecars. The incremental
//! cycle drives [`rebuild_pages`] against the dirty set produced by
//! [`crate::incremental::IncrementalCycle`] and the prior manifest using
//! the stateless [`mars_source::Source::stream_rows_by_id`] surface; it
//! additionally refreshes the per-binding page-membership sidecar so the
//! next cycle's old-side lookups resolve correctly.
//!
//! Truncate fall-back: when a binding is marked truncated the executor
//! delegates to the bootstrap path for that binding only, so a single
//! [`RebuildOutcome`] carries incremental and bootstrap-class work alike
//! into the manifest commit.
//!
//! Concurrency: each entry point processes one binding at a time. Per-
//! binding parallelism is the cycle entry point's responsibility; here we
//! only require that one call runs at a time per binding.
//!
//! Sidecar threshold: the encoded page-membership sidecar is checked
//! against the binding's `sidecar_size_warn_bytes`; an exceedance fires a
//! `tracing::warn!` plus a metric counter without blocking the cycle.
//!
//! The implementation is split across submodules: [`pass2`] owns the
//! streaming row→page bucketing for [`rebuild_binding_from_plan`];
//! [`flush`] owns the page-complete artifact + sidecar emission used by
//! every path; [`rebalance`] owns the Split / Merge executor.

mod flush;
mod page_accumulator;
mod pass2;
mod rebalance;

pub use pass2::rebuild_binding_from_plan;
pub use rebalance::execute_rebalance;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::StreamExt;
use mars_artifact::{AttrValue as ArtAttrValue, FeatureGeom, compute_content_hash, wkb_to_feature_geom};
use mars_source::{AttrValue, RowBytes, SourceBinding as PortBinding, SourceCollectionId, SourceError};
use mars_types::{
    ArtifactEntry, ArtifactKey, Bbox, BindingId, BindingMetadata, ContentHash, DecimationLevel, HilbertKey,
    LayerSidecarEntry, LevelMetadata, Manifest, PageEntry, PageId,
};

use crate::disk_governor::DiskGovernor;
use crate::incremental::{BindingDirty, DirtyPages};
use crate::memory_governor::MemoryGovernor;
use crate::page_plan::compute_page_plan;
use crate::plan::{BootstrapPlan, LevelPlan};
use crate::sidecar::{SidecarReader, encode_sidecar};
use crate::{CompilerError, Deps};

use self::flush::{emit_layer_sidecars, filter_unmatched_rows, flush_page};

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
    /// from the manifest. A missing page is a missing page;
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

impl RebuildOutcome {
    /// Move every entry from `other` into `self`. Used by `rebuild_pages`
    /// to merge a per-binding local outcome into the shared one after the
    /// binding's rebuild succeeds; on failure the local is dropped instead.
    pub fn absorb(&mut self, mut other: RebuildOutcome) {
        self.replacement_pages.append(&mut other.replacement_pages);
        self.dropped_pages.append(&mut other.dropped_pages);
        self.replacement_class_sidecars
            .append(&mut other.replacement_class_sidecars);
        self.replacement_label_sidecars
            .append(&mut other.replacement_label_sidecars);
        self.dropped_class_sidecars.append(&mut other.dropped_class_sidecars);
        self.dropped_label_sidecars.append(&mut other.dropped_label_sidecars);
        self.refreshed_bindings.append(&mut other.refreshed_bindings);
    }
}

/// Output of one binding compile through the unified pipeline.
#[derive(Debug)]
pub struct BindingOutput {
    pub meta: BindingMetadata,
    pub pages: Vec<PageEntry>,
    pub class_sidecars: Vec<LayerSidecarEntry>,
    pub label_sidecars: Vec<LayerSidecarEntry>,
}

// -- per-page render helpers --------------------------------------------
//
// shared by the unified compile pipeline (truncate + bootstrap-from-plan
// via `rebuild_binding_from_plan`), the incremental rebuild path, and the
// rebalance executor. one source row decoded into a feature, with attrs
// preserved for class / label evaluation and a hilbert key over the
// binding's combined bbox.
//
// `row_fingerprint` is BLAKE3 over WKB truncated to u64, used as the final
// tiebreaker after `(hilbert_key, user_id)`. Within a `(key, user_id, WKB)`
// tie attribute differences are NOT order-stable: rows with identical
// geometry but different attrs hash to the same fingerprint and are
// treated as equivalent for slot ordering. The page-rebuild pipeline
// re-encodes attributes from the freshly hydrated row regardless, so the
// substrate stays consistent.
#[derive(Debug, Clone)]
pub(crate) struct KeyedRow {
    pub(crate) feature: FeatureGeom,
    pub(crate) attrs: Arc<Vec<(String, AttrValue)>>,
    pub(crate) geom_bytes_estimate: u64,
    pub(crate) key: HilbertKey,
    pub(crate) row_fingerprint: u64,
}

/// Drain a row stream into deterministic-ordered [`KeyedRow`]s with hilbert
/// keys assigned over `combined_bbox`. Shared by the incremental, rebalance,
/// and (step 6) bootstrap-from-plan paths so all three hydrate rows
/// identically.
///
/// Memory budgets are enforced per-page by the caller (see
/// [`enforce_page_budget`]) - the hydration step itself is unbounded
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
        let row_fingerprint = compute_row_fingerprint_from_wkb(&row.geometry);
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
    let mut observed: u64 = 0;
    for r in rows {
        let attr_bytes: u64 = r.attrs.iter().map(|(k, _)| (k.len() + 16) as u64).sum();
        let est = r.geom_bytes_estimate.saturating_add(attr_bytes).saturating_add(64);
        observed = observed.saturating_add(est);
        if observed > working_set_bytes {
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

/// Stable per-row tiebreaker. BLAKE3 over geometry bytes truncated to u64.
/// Identical WKB → identical fingerprint regardless of attribute payload;
/// attribute differences within a `(key, user_id, WKB)` tie are not
/// order-stable.
pub(crate) fn compute_row_fingerprint_from_wkb(wkb: &[u8]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(wkb);
    let mut out = [0u8; 8];
    hasher.finalize_xof().fill(&mut out);
    u64::from_le_bytes(out)
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

/// Run one rebuild pass for the given dirty set. Per-binding sidecar
/// thresholds are read from the matching [`BindingPlan`].
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
/// [`BindingFailurePolicy::Isolate`] a per-binding error is logged,
/// metered, and discarded so the cycle still publishes the other
/// bindings' progress; under [`BindingFailurePolicy::FailCycle`] the
/// first error aborts the pass (under concurrent execution, "first" is
/// whichever in-flight rebuild errors first - non-deterministic across
/// runs).
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
    let layer_plans: Vec<&crate::plan::LayerPlan> = plan.layers_for(&binding_plan.binding_id).collect();
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

/// Recompute level metadata after pages were replaced or dropped. Pure;
/// runs at the cycle entry point after all rebuilds finish, against the
/// merged page list. Exposed here rather than at the cycle entry point
/// because it is the natural complement to [`rebuild_pages`].
#[must_use]
pub fn recompute_level_metadata(prior: &LevelMetadata, pages: &[PageEntry], binding_id: &BindingId) -> LevelMetadata {
    let mut ranges: Vec<(HilbertKey, HilbertKey, PageId)> = pages
        .iter()
        .filter(|p| p.key.binding_id == *binding_id && p.key.level == prior.level)
        .map(|p| (p.hilbert_range.0, p.hilbert_range.1, p.key.page_id))
        .collect();
    ranges.sort_by_key(|r| r.0);
    LevelMetadata {
        level: prior.level,
        vertex_tolerance_m: prior.vertex_tolerance_m,
        geometry_min_size_m: prior.geometry_min_size_m,
        label_min_priority: prior.label_min_priority,
        page_count: ranges.len() as u32,
        hilbert_range_table: ranges,
    }
}

pub(crate) fn empty_level_metadata(level: &LevelPlan) -> LevelMetadata {
    LevelMetadata {
        level: level.level,
        vertex_tolerance_m: level.vertex_tolerance_m,
        geometry_min_size_m: level.geometry_min_size_m,
        label_min_priority: level.label_min_priority,
        page_count: 0,
        hilbert_range_table: Vec::new(),
    }
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

pub(crate) fn attr_value_to_artifact(v: &AttrValue) -> ArtAttrValue {
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
                (HilbertKey::new(50), HilbertKey::new(75), PageId::new(1)),
                (HilbertKey::new(100), HilbertKey::new(200), PageId::new(0)),
            ]
        );
    }
}

//! Pass 2 of the unified compile flow: stream the bound table once per
//! binding and bucket rows into the planned `(level, page_id)` targets via
//! [`mars_source::SourceRowKey`]. Completed pages eager-flush via
//! [`super::flush::flush_one_page`]; partial buffers spill to disk when the
//! shared in-flight budget trips.

use std::sync::Arc;

use futures_util::StreamExt;
use mars_artifact::{FeatureGeom, compute_content_hash, wkb_to_feature_geom};
use mars_source::RowBytes;
use mars_types::{ArtifactEntry, BindingMetadata, LayerSidecarEntry, LevelMetadata, PageEntry, PageId};

use crate::decimate::{passes_min_size, simplify};
use crate::disk_governor::DiskGovernor;
use crate::memory_governor::MemoryGovernor;
use crate::page_plan::PagePlan;
use crate::plan::{BindingPlan, BootstrapPlan, LayerPlan};
use crate::sidecar::encode_sidecar;
use crate::{CompilerError, Deps};

use super::flush::flush_one_page;
use super::page_accumulator::PageAccumulator;
use super::{
    BindingOutput, KeyedRow, compute_row_fingerprint_from_wkb, empty_level_metadata, membership_sidecar_object_key,
};

/// Accumulator for one binding's pass-2 output. Owns the per-level page
/// vectors and sidecar lists so the row-routing loop only sees a single
/// `flush_page` entry point.
struct BindingOutputBuilder<'a> {
    binding_plan: &'a BindingPlan,
    layer_plans: Vec<&'a LayerPlan>,
    levels_pages: Vec<Vec<PageEntry>>,
    class_sidecars: Vec<LayerSidecarEntry>,
    label_sidecars: Vec<LayerSidecarEntry>,
}

impl<'a> BindingOutputBuilder<'a> {
    fn new(binding_plan: &'a BindingPlan, layer_plans: Vec<&'a LayerPlan>) -> Self {
        let levels_pages = vec![Vec::new(); binding_plan.levels.len()];
        Self {
            binding_plan,
            layer_plans,
            levels_pages,
            class_sidecars: Vec::new(),
            label_sidecars: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn flush_page(
        &mut self,
        deps: &Deps,
        lvl_idx: usize,
        page_id: PageId,
        kept: Vec<KeyedRow>,
        pruned: Vec<KeyedRow>,
        working_set_bytes: u64,
        spill_dir: &std::path::Path,
        governor: &MemoryGovernor,
    ) -> Result<(), CompilerError> {
        flush_one_page(
            deps,
            self.binding_plan,
            lvl_idx,
            page_id,
            kept,
            pruned,
            &self.layer_plans,
            working_set_bytes,
            spill_dir,
            governor,
            &mut self.levels_pages,
            &mut self.class_sidecars,
            &mut self.label_sidecars,
        )
        .await
    }

    fn into_parts(self) -> (Vec<Vec<PageEntry>>, Vec<LayerSidecarEntry>, Vec<LayerSidecarEntry>) {
        (self.levels_pages, self.class_sidecars, self.label_sidecars)
    }
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
#[allow(clippy::too_many_arguments)]
pub async fn rebuild_binding_from_plan<'a>(
    deps: &Deps,
    plan: &BootstrapPlan,
    binding_plan: &BindingPlan,
    page_plan: &PagePlan,
    session: &mut (dyn mars_source::CompileSession + 'a),
    working_set_bytes: u64,
    in_flight_budget_bytes: u64,
    spill_dir: &std::path::Path,
    spill_open_file_limit: usize,
    governor: &MemoryGovernor,
    disk_governor: &DiskGovernor,
) -> Result<BindingOutput, CompilerError> {
    if page_plan.feature_count_total == 0 {
        return Ok(BindingOutput {
            meta: BindingMetadata {
                binding_id: binding_plan.binding_id.clone(),
                source_table: binding_plan.source_table.clone(),
                native_crs: binding_plan.native_crs.clone(),
                feature_count_total: 0,
                combined_bbox: page_plan.combined_bbox,
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

    let rebuild_started = std::time::Instant::now();
    tracing::info!(
        target: "mars_compiler::compile",
        binding = %binding_plan.binding_id,
        levels = binding_plan.levels.len(),
        feature_count_total = page_plan.feature_count_total,
        "compile.rebuild.start",
    );

    // pass-2 streams the bound table once per binding and buckets rows
    // into the planned (level, page_id) targets via SourceRowKey. one
    // sequential heap scan replaces the historical per-page WHERE id =
    // ANY($1) pattern, whose heap-walk cost dominated compile time on
    // large bindings. each row decodes its WKB exactly once and Arc::clones
    // its attribute payload across multi-level fanout. completed pages
    // eager-flush so partial buffers don't pile up.
    let layer_plans: Vec<&LayerPlan> = plan.layers_for(&binding_plan.binding_id).collect();
    let mut builder = BindingOutputBuilder::new(binding_plan, layer_plans);

    let mut targets = crate::route_index::RouteIndex::with_governor(governor, spill_dir)?;
    let mut acc = PageAccumulator::new();
    for (lvl_idx, level_pp) in page_plan.levels.iter().enumerate() {
        for planned in &level_pp.pages {
            if planned.row_keys.len() != planned.feature_ids.len() {
                return Err(CompilerError::InvariantViolation {
                    what: "rebuild_from_plan: planned row_keys / feature_ids length mismatch",
                });
            }
            acc.set_expected(lvl_idx, planned.page_id, planned.row_keys.len());
            for rk in &planned.row_keys {
                targets.insert(*rk, (lvl_idx, planned.page_id))?;
            }
        }
    }
    // freeze the build-phase route table into a single sorted file and
    // walk it as a forward cursor alongside the pass-2 row stream. the
    // pass-2 SQL is pinned to single-worker heap-scan order (BE row_key
    // == numeric tableoid/block/offset ascending), so the cursor is
    // strictly monotonic in the row_key space.
    let (mut targets, freeze_stats) = targets.freeze()?;
    tracing::info!(
        target: "mars_compiler::compile",
        binding = %binding_plan.binding_id,
        entries_total = freeze_stats.entries_total,
        runs_merged = freeze_stats.runs_merged,
        bytes_written = freeze_stats.bytes_written,
        elapsed_ms = freeze_stats.elapsed_ms,
        "compile.route_index.freeze",
    );

    let mut spill = crate::spill::SpillManager::new(spill_dir, spill_open_file_limit)?;

    let mut stream = session.stream_rows().await?;
    while let Some(item) = stream.next().await {
        let row: RowBytes = item?;
        // a row whose pass-1 bbox failed every level's filter has no route
        // and is silently skipped.
        let Some(routes) = targets.advance_to(&row.row_key)? else {
            continue;
        };

        let geom_bytes_estimate = row.geometry.len() as u64;
        let row_fingerprint = compute_row_fingerprint_from_wkb(&row.geometry);
        let feature = wkb_to_feature_geom(&row.geometry, row.feature_id)?;
        let cx = (f64::from(feature.bbox[0]) + f64::from(feature.bbox[2])) / 2.0;
        let cy = (f64::from(feature.bbox[1]) + f64::from(feature.bbox[3])) / 2.0;
        let key = crate::hilbert::key_from_centroid(cx, cy, page_plan.combined_bbox);
        let attrs = Arc::new(row.attributes);
        let attr_bytes: u64 = attrs.iter().map(|(k, _)| (k.len() + 16) as u64).sum();
        let kr_bytes = geom_bytes_estimate.saturating_add(attr_bytes).saturating_add(64);

        for (lvl_idx, page_id) in routes {
            let level_plan = &binding_plan.levels[lvl_idx];
            let (kept_row, pruned_row) = if passes_min_size(&feature, level_plan.geometry_min_size_m) {
                let simplified = simplify(&feature.geom, level_plan.vertex_tolerance_m, binding_plan.simplifier);
                (
                    Some(KeyedRow {
                        feature: FeatureGeom {
                            user_id: feature.user_id,
                            bbox: feature.bbox,
                            geom: simplified,
                        },
                        attrs: attrs.clone(),
                        geom_bytes_estimate,
                        key,
                        row_fingerprint,
                    }),
                    None,
                )
            } else {
                (
                    None,
                    Some(KeyedRow {
                        feature: feature.clone(),
                        attrs: attrs.clone(),
                        geom_bytes_estimate,
                        key,
                        row_fingerprint,
                    }),
                )
            };

            if spill.is_spilled(lvl_idx, page_id) {
                // page already on disk: append directly, no in-flight bookkeeping.
                if let Some(r) = &kept_row {
                    spill
                        .append(lvl_idx, page_id, crate::spill::SpillKind::Kept, r, disk_governor)
                        .await?;
                }
                if let Some(r) = &pruned_row {
                    spill
                        .append(lvl_idx, page_id, crate::spill::SpillKind::Pruned, r, disk_governor)
                        .await?;
                }
            } else {
                acc.push(lvl_idx, page_id, kept_row, pruned_row, kr_bytes, governor);
            }

            // page complete: drain its buffers (memory and/or spill), write
            // the artifact + sidecars, reclaim its working-set footprint.
            if acc.record_arrival(lvl_idx, page_id) {
                let (kept, dropped) = if spill.is_spilled(lvl_idx, page_id) {
                    let (mut k, mut d) = spill.drain(lvl_idx, page_id)?;
                    let (extra_k, extra_d) = acc.take(lvl_idx, page_id);
                    k.extend(extra_k);
                    d.extend(extra_d);
                    (k, d)
                } else {
                    acc.take(lvl_idx, page_id)
                };
                builder
                    .flush_page(
                        deps,
                        lvl_idx,
                        page_id,
                        kept,
                        dropped,
                        working_set_bytes,
                        spill_dir,
                        governor,
                    )
                    .await?;
            }
        }

        // soft trigger: when the in-memory partial-page set crosses the
        // budget, evict everything to per-page spill files. subsequent rows
        // for those pages append directly to disk.
        if acc.in_flight_bytes() > in_flight_budget_bytes {
            acc.evict_to_spill(&mut spill, disk_governor).await?;
        }
    }

    acc.verify_complete()?;

    let (mut levels_pages, class_sidecars, label_sidecars) = builder.into_parts();

    // build per-level metadata in plan order, restoring the level summary
    // events that the per-level loop used to emit.
    let mut levels_meta: Vec<LevelMetadata> = Vec::with_capacity(binding_plan.levels.len());
    let mut all_pages: Vec<PageEntry> = Vec::new();
    for (lvl_idx, level_plan) in binding_plan.levels.iter().enumerate() {
        let mut level_pages = std::mem::take(&mut levels_pages[lvl_idx]);
        level_pages.sort_by_key(|p| p.key.page_id);
        tracing::info!(
            target: "mars_compiler::compile",
            binding = %binding_plan.binding_id,
            level = level_plan.level.get(),
            pages_emitted = level_pages.len(),
            "compile.level.summary",
        );
        levels_meta.push(LevelMetadata {
            level: level_plan.level,
            vertex_tolerance_m: level_plan.vertex_tolerance_m,
            geometry_min_size_m: level_plan.geometry_min_size_m,
            label_min_priority: level_plan.label_min_priority,
            page_count: level_pages.len() as u32,
            hilbert_range_table: level_pages
                .iter()
                .map(|p| (p.hilbert_range.0, p.hilbert_range.1, p.key.page_id))
                .collect(),
        });
        all_pages.append(&mut level_pages);
    }

    // page-membership sidecar: pass-1 already collected (user_id, hilbert_key)
    // for every unfiltered row, so we just hand the slice to encode_sidecar.
    let mut sidecar_entries = page_plan.sidecar_arena.drain_into_vec()?;
    let sidecar_bytes = encode_sidecar(&mut sidecar_entries)?;
    let sidecar_hash = compute_content_hash(&sidecar_bytes);
    let sidecar_key = membership_sidecar_object_key(binding_plan.binding_id.as_str(), &sidecar_hash)?;
    let sidecar_size = sidecar_bytes.len() as u64;
    if sidecar_size > binding_plan.sidecar_size_warn_bytes {
        tracing::warn!(
            binding = binding_plan.binding_id.as_str(),
            size_bytes = sidecar_size,
            threshold_bytes = binding_plan.sidecar_size_warn_bytes,
            "page-membership sidecar exceeds warning threshold; consider REPLICA IDENTITY FULL for this binding"
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
        combined_bbox: page_plan.combined_bbox,
        levels: levels_meta,
        page_membership_sidecar: Some(ArtifactEntry {
            key: sidecar_key,
            hash: sidecar_hash,
            size_bytes: sidecar_size,
        }),
    };
    let output = BindingOutput {
        meta,
        pages: all_pages,
        class_sidecars,
        label_sidecars,
    };
    let spill_metrics = spill.metrics();
    tracing::info!(
        target: "mars_compiler::compile",
        binding = %binding_plan.binding_id,
        elapsed_ms = rebuild_started.elapsed().as_millis() as u64,
        pages = output.pages.len(),
        class_sidecars = output.class_sidecars.len(),
        label_sidecars = output.label_sidecars.len(),
        spill_triggered = spill_metrics.triggered,
        spill_bytes_written = spill_metrics.bytes_written,
        spill_bytes_read = spill_metrics.bytes_read,
        spill_files_active_peak = spill_metrics.files_active_peak,
        governor_peak_bytes = governor.peak_bytes(),
        governor_acquire_wait_us = governor.acquire_wait_us(),
        "compile.rebuild.end",
    );
    Ok(output)
}

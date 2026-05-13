//! Rebalance executor: apply Split / Merge ops by re-fetching the affected
//! features through the page-membership sidecar and emitting fresh page
//! artifacts plus class / label sidecars.

use std::collections::{BTreeMap, HashMap, HashSet};

use mars_artifact::FeatureGeom;
use mars_source::{SourceBinding as PortBinding, SourceCollectionId};
use mars_types::{BindingId, DecimationLevel, HilbertKey, PageEntry, PageId};

use crate::decimate::{passes_min_size, simplify};
use crate::plan::{BootstrapPlan, LayerPlan};
use crate::rebalance::RebalanceOp;
use crate::sidecar::SidecarReader;
use crate::{CompilerError, Deps};

use super::flush::{emit_layer_sidecars, filter_unmatched_rows, flush_page};
use super::{KeyedRow, RebuildOutcome, drain_pruned_through, enforce_page_budget, hydrate_keyed_rows};

/// Apply a list of [`RebalanceOp`]s, fetching the affected feature ids via
/// `Source::stream_rows_by_id` and emitting fresh page artifacts plus
/// class / label sidecars. Source pages are dropped; replacement pages are
/// allocated fresh `PageId`s above the existing maximum at the affected
/// (binding, level). The page-membership sidecar is left untouched -- a
/// rebalance preserves every feature_id and its hilbert key.
pub async fn execute_rebalance(
    deps: &Deps,
    plan: &BootstrapPlan,
    prior: &mars_types::Manifest,
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
    prior: &mars_types::Manifest,
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
    // bag semantics: dedup user_ids before the source fetch - a user_id
    // that appears N times in the multimap should still be fetched once,
    // since the source returns ALL its rows.
    let mut feature_ids = sc.user_ids_in_ranges(&union_ranges);
    feature_ids.sort_unstable();
    feature_ids.dedup();

    // fetch rows.
    let port_binding = PortBinding::new(
        SourceCollectionId::new(binding_plan.binding_id.as_str()),
        binding_plan.source_table.clone(),
        binding_plan.geometry_field.clone(),
        binding_plan.id_field.as_deref().unwrap_or("id"),
        binding_plan.attributes.clone(),
        binding_plan.native_crs.clone(),
    )?
    .with_filter(binding_plan.filter.clone());
    let ids: Vec<i64> = feature_ids
        .iter()
        .map(|f| i64::try_from(*f).unwrap_or(i64::MAX))
        .collect();
    let stream = deps.source.stream_rows_by_id(&port_binding, &ids).await?;
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
                    // β.2: drop rows no layer's class chain matches before
                    // geometry emit (rebalance Split).
                    let (slice, dropped_unmatched) = filter_unmatched_rows(slice, &layer_plans);
                    if dropped_unmatched > 0 {
                        deps.metrics
                            .inc_compiler_features_unmatched(binding_plan.binding_id.as_str(), dropped_unmatched);
                    }
                    if slice.is_empty() {
                        continue;
                    }
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
                // β.2: drop rows no layer's class chain matches before geometry
                // emit (rebalance Merge).
                let (merged_leveled, dropped_unmatched) = filter_unmatched_rows(merged_leveled, &layer_plans);
                if dropped_unmatched > 0 {
                    deps.metrics
                        .inc_compiler_features_unmatched(binding_plan.binding_id.as_str(), dropped_unmatched);
                }
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

//! Page-complete flush: encode an artifact, write it to the store, and emit
//! per-layer class / label sidecars. Shared by the unified pass-2 pipeline,
//! the incremental rebuild path, and the rebalance executor.

use bytes::Bytes;
use mars_artifact::{
    ArtifactKind, ArtifactWriter, AttrValue as ArtAttrValue, FeatureGeom, LabelCandidate, MAX_ROW_BYTES,
    SpatialIndexBuilder, compute_content_hash, encode_row,
};
use mars_types::{Bbox, DecimationLevel, HilbertKey, LayerSidecarEntry, LayerSidecarKind, PageEntry, PageId, PageKey};

use crate::class_eval::{LabelSpec, RowAttrs, assign_class, emit_label_candidate};
use crate::external_sort::external_sort_page;
use crate::memory_governor::MemoryGovernor;
use crate::plan::{BindingPlan, LayerPlan, LevelPlan};
use crate::{CompilerError, Deps};

use super::{KeyedRow, attr_value_to_artifact};

#[allow(clippy::too_many_arguments)]
pub(super) async fn flush_one_page(
    deps: &Deps,
    binding_plan: &BindingPlan,
    lvl_idx: usize,
    page_id: PageId,
    page_rows: Vec<KeyedRow>,
    pruned_rows: Vec<KeyedRow>,
    layer_plans: &[&LayerPlan],
    working_set_bytes: u64,
    spill_dir: &std::path::Path,
    governor: &MemoryGovernor,
    levels_pages: &mut [Vec<PageEntry>],
    class_sidecars: &mut Vec<LayerSidecarEntry>,
    label_sidecars: &mut Vec<LayerSidecarEntry>,
) -> Result<(), CompilerError> {
    let level_plan = &binding_plan.levels[lvl_idx];
    // governor-bounded sort: in-memory fast path when the page footprint
    // fits the cap, chunked-spill k-way-merge slow path otherwise. byte-
    // identical output to today's `Vec::sort_by` either way.
    let page_rows = external_sort_page(page_rows, working_set_bytes, spill_dir, governor)?;
    // β.2: drop rows no layer's class chain matches before geometry emit.
    let (page_rows, dropped_unmatched) = filter_unmatched_rows(page_rows, layer_plans);
    if dropped_unmatched > 0 {
        deps.metrics
            .inc_compiler_features_unmatched(binding_plan.binding_id.as_str(), dropped_unmatched);
    }
    let _ = page_id;
    let _ = working_set_bytes;
    if page_rows.is_empty() {
        // pruned-only page: drop entirely, matches incremental contract.
        return Ok(());
    }
    let page_started = std::time::Instant::now();
    let row_count = page_rows.len();
    let entry = flush_page(deps, binding_plan, level_plan.level, page_id, &page_rows).await?;
    emit_layer_sidecars(
        deps,
        level_plan,
        &entry,
        &page_rows,
        &pruned_rows,
        layer_plans,
        class_sidecars,
        label_sidecars,
    )
    .await?;
    tracing::info!(
        target: "mars_compiler::compile",
        binding = %binding_plan.binding_id,
        level = level_plan.level.get(),
        page_id = page_id.get(),
        rows = row_count,
        bytes = entry.size_bytes,
        elapsed_ms = page_started.elapsed().as_millis() as u64,
        "compile.page.flush",
    );
    levels_pages[lvl_idx].push(entry);
    Ok(())
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

/// drop rows that no layer's class chain matches. a row is kept if at
/// least one layer either has no classes (label-only layers can't drop)
/// or matches via [`assign_class`]. keeps the geometry payload tight:
/// features that would silently drop at render time (counted in
/// `mars_render_feature_unstyled_total`) never reach the artifact.
///
/// returns `(kept, dropped_count)`. order of kept rows is preserved.
pub(crate) fn filter_unmatched_rows(rows: Vec<KeyedRow>, layers: &[&LayerPlan]) -> (Vec<KeyedRow>, u64) {
    if layers.is_empty() || layers.iter().any(|l| l.classes.is_empty()) {
        // a label-only layer (or no layers at all) cannot drop rows at this
        // pass: we have no class chain authoritative enough to decide. keep
        // everything; runtime stays defensive via the unstyled counter.
        return (rows, 0);
    }
    let per_layer: Vec<Vec<Option<mars_expr::Expr>>> = layers
        .iter()
        .map(|l| l.classes.iter().map(|c| c.when.clone()).collect())
        .collect();
    let mut dropped: u64 = 0;
    let kept: Vec<KeyedRow> = rows
        .into_iter()
        .filter(|r| {
            let attrs = RowAttrs::new(r.attrs.as_ref());
            let any_match = per_layer.iter().any(|wc| assign_class(wc, &attrs).is_some());
            if !any_match {
                dropped += 1;
            }
            any_match
        })
        .collect();
    (kept, dropped)
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
        let LayerStyleRefs {
            style_refs_full,
            class_label_indices,
            layer_label_index,
        } = build_layer_style_refs(layer)?;

        let class_label_specs: Vec<Option<LabelSpec>> = layer
            .classes
            .iter()
            .zip(class_label_indices.iter())
            .map(|(class, idx)| {
                class.label.as_ref().zip(*idx).map(|(plan, style_ref_idx)| LabelSpec {
                    priority: plan.style.priority,
                    text: &plan.text,
                    placement: &plan.placement,
                    style_ref_idx,
                })
            })
            .collect();
        let layer_label_spec = layer
            .label
            .as_ref()
            .zip(layer_label_index)
            .map(|(l, style_ref_idx)| LabelSpec {
                priority: l.style.priority,
                text: &l.text,
                placement: &l.placement,
                style_ref_idx,
            });

        // class-overrides-layer: pick the per-class spec when set, else fall
        // back to the layer-level spec. classes without their own label and
        // no layer fallback contribute no label candidate.
        let label_spec_for = |class_idx: Option<u16>| -> Option<&LabelSpec> {
            if let Some(idx) = class_idx
                && let Some(Some(spec)) = class_label_specs.get(idx as usize)
            {
                return Some(spec);
            }
            layer_label_spec.as_ref()
        };

        for (slot, r) in rows.iter().enumerate() {
            let slot_u32 = u32::try_from(slot).map_err(|_| CompilerError::InvariantViolation {
                what: "page slot overflow",
            })?;
            let attrs = RowAttrs::new(r.attrs.as_ref());
            let class_idx = assign_class(&when_clauses, &attrs);
            if let Some(idx) = class_idx {
                assignments.push((slot_u32, idx));
            }
            if let Some(spec) = label_spec_for(class_idx)
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

        // β.3 invariant: when the binding hosts exactly one classed layer
        // (the typical fixture shape), every emitted geometry slot must
        // have a class assignment after β.2's drop-at-emit filter. shared-
        // binding pages legitimately leave per-layer sidecars sparse, so
        // they're exempt from the assertion.
        let classed_layers = layers.iter().filter(|l| !l.classes.is_empty()).count();
        if classed_layers == 1 && !layer.classes.is_empty() && assignments.len() != rows.len() {
            return Err(CompilerError::ClassGeometryMismatch {
                layer: layer.layer_id.as_str().to_owned(),
                page: page.key.page_id,
                geom: rows.len(),
                class: assignments.len(),
            });
        }

        // pruned features keep the Independent-survival contract: evaluate
        // their class too so per-class labels stick when the geometry was
        // dropped by the level's size filter.
        for r in pruned {
            let attrs = RowAttrs::new(r.attrs.as_ref());
            let class_idx = assign_class(&when_clauses, &attrs);
            if let Some(spec) = label_spec_for(class_idx)
                && let Some(c) = emit_label_candidate(
                    &r.feature,
                    None,
                    &attrs,
                    spec,
                    layer.label_survival,
                    level.label_min_priority,
                )
            {
                labels.push(c);
            }
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

/// Layout of the per-layer `style_refs` array published in the class sidecar.
/// Geometry style names come first (one per class, in class order); per-class
/// label style names follow (one slot per class that carries a CLASS-level
/// LABEL, in class order); finally the optional layer-level label style.
///
/// `class_label_indices[i]` is `Some(idx)` exactly when class i has a
/// per-class label; `idx` is the position of that class's label style in
/// `style_refs_full`. `layer_label_index` is `Some(idx)` exactly when the
/// layer carries a layer-level label.
struct LayerStyleRefs {
    style_refs_full: Vec<String>,
    class_label_indices: Vec<Option<u16>>,
    layer_label_index: Option<u16>,
}

fn build_layer_style_refs(layer: &LayerPlan) -> Result<LayerStyleRefs, CompilerError> {
    let mut style_refs_full: Vec<String> = layer.classes.iter().map(|c| c.style_ref.clone()).collect();
    let mut class_label_indices: Vec<Option<u16>> = Vec::with_capacity(layer.classes.len());
    for class in &layer.classes {
        match &class.label {
            Some(plan) => {
                let idx = u16::try_from(style_refs_full.len()).map_err(|_| CompilerError::InvariantViolation {
                    what: "per-class label style_ref_idx exceeds u16::MAX",
                })?;
                style_refs_full.push(plan.style_ref.clone());
                class_label_indices.push(Some(idx));
            }
            None => class_label_indices.push(None),
        }
    }
    let layer_label_index = match layer.label.as_ref() {
        Some(l) => {
            let idx = u16::try_from(style_refs_full.len()).map_err(|_| CompilerError::InvariantViolation {
                what: "layer label style_ref_idx exceeds u16::MAX (config validation should have rejected this)",
            })?;
            style_refs_full.push(l.style_ref.clone());
            Some(idx)
        }
        None => None,
    };
    Ok(LayerStyleRefs {
        style_refs_full,
        class_label_indices,
        layer_label_index,
    })
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_artifact::FeatureGeom;
    use mars_source::AttrValue;
    use mars_types::{BindingId, LayerId};
    use std::sync::Arc;

    fn keyed_row(user_id: u64, kind: &str, key: u64) -> KeyedRow {
        KeyedRow {
            feature: FeatureGeom {
                user_id,
                bbox: [0.0, 0.0, 1.0, 1.0],
                geom: mars_artifact::GeomKind::Point((0.0, 0.0)),
            },
            attrs: Arc::new(vec![("kind".into(), AttrValue::String(kind.into()))]),
            geom_bytes_estimate: 16,
            key: HilbertKey::new(key),
            row_fingerprint: user_id,
        }
    }

    fn layer_with_classes(name: &str, when_exprs: &[Option<&str>]) -> crate::plan::LayerPlan {
        let classes = when_exprs
            .iter()
            .enumerate()
            .map(|(i, w)| crate::plan::ClassPlan {
                name: format!("c{i}"),
                when: w.map(|s| mars_expr::parse(s).unwrap()),
                style_ref: format!("{name}__c{i}"),
                label: None,
            })
            .collect();
        crate::plan::LayerPlan {
            layer_id: LayerId::new(name),
            binding_id: BindingId::try_new(name).unwrap(),
            kind: "geom".into(),
            classes,
            label: None,
            label_survival: mars_style::LabelSurvival::Independent,
        }
    }

    #[test]
    fn filter_unmatched_rows_drops_rows_that_match_no_layer() {
        let layer = layer_with_classes("roads", &[Some("kind = 'major'")]);
        let layers: Vec<&crate::plan::LayerPlan> = vec![&layer];
        let rows = vec![
            keyed_row(1, "major", 10),
            keyed_row(2, "minor", 20),
            keyed_row(3, "major", 30),
        ];
        let (kept, dropped) = filter_unmatched_rows(rows, &layers);
        assert_eq!(dropped, 1);
        let ids: Vec<u64> = kept.iter().map(|r| r.feature.user_id).collect();
        assert_eq!(ids, vec![1, 3]);
    }

    #[test]
    fn filter_unmatched_rows_keeps_all_when_a_layer_has_no_classes() {
        // a label-only layer (no classes) means we cannot authoritatively
        // drop anything: keep all rows so its labels still emit.
        let label_only = crate::plan::LayerPlan {
            layer_id: LayerId::new("labels"),
            binding_id: BindingId::try_new("labels").unwrap(),
            kind: "geom".into(),
            classes: Vec::new(),
            label: None,
            label_survival: mars_style::LabelSurvival::Independent,
        };
        let layers: Vec<&crate::plan::LayerPlan> = vec![&label_only];
        let rows = vec![keyed_row(1, "anything", 10), keyed_row(2, "else", 20)];
        let (kept, dropped) = filter_unmatched_rows(rows, &layers);
        assert_eq!(dropped, 0);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn filter_unmatched_rows_keeps_row_that_matches_any_layer() {
        // shared-binding case: layer A matches "major", layer B matches
        // "minor". a row labelled "minor" must survive because B keeps it.
        let a = layer_with_classes("a", &[Some("kind = 'major'")]);
        let b = layer_with_classes("b", &[Some("kind = 'minor'")]);
        let layers: Vec<&crate::plan::LayerPlan> = vec![&a, &b];
        let rows = vec![
            keyed_row(1, "major", 10),
            keyed_row(2, "minor", 20),
            keyed_row(3, "path", 30),
        ];
        let (kept, dropped) = filter_unmatched_rows(rows, &layers);
        assert_eq!(dropped, 1);
        let ids: Vec<u64> = kept.iter().map(|r| r.feature.user_id).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn filter_unmatched_rows_keeps_all_under_catch_all_class() {
        // a `None` when-clause is the catch-all; assign_class returns Some
        // for it, so no row should be dropped.
        let layer = layer_with_classes("any", &[None]);
        let layers: Vec<&crate::plan::LayerPlan> = vec![&layer];
        let rows = vec![keyed_row(1, "x", 10), keyed_row(2, "y", 20)];
        let (kept, dropped) = filter_unmatched_rows(rows, &layers);
        assert_eq!(dropped, 0);
        assert_eq!(kept.len(), 2);
    }

    fn label_plan(style_ref: &str) -> crate::plan::LayerLabelPlan {
        crate::plan::LayerLabelPlan {
            style_ref: style_ref.into(),
            style: mars_style::LabelStyle {
                font_family: "DejaVu Sans".into(),
                font_size: 12.0.into(),
                fill: mars_style::Colour::rgb(0, 0, 0),
                halo: None,
                priority: 0,
                min_distance: 0.0,
                position: mars_style::AnchorPosition::default(),
                offset_px: (0.0, 0.0),
                angle_deg: None,
                partials: false,
                force: false,
            },
            text: mars_expr::parse_template("{name}").unwrap(),
            placement: mars_style::Placement::Point,
        }
    }

    #[test]
    fn style_refs_layout_geom_then_class_labels_then_layer_label() {
        // shape: 3 classes; class 0 and class 2 have per-class labels; layer
        // has a fallback label. expected style_refs order:
        // [geom0, geom1, geom2, classlabel0, classlabel2, layerlabel].
        let mut layer = layer_with_classes("vejnavne", &[Some("kind = 'major'"), None, None]);
        layer.classes[0].label = Some(label_plan("vejnavne__major__label"));
        layer.classes[2].label = Some(label_plan("vejnavne__other__label"));
        layer.label = Some(label_plan("vejnavne__label"));

        let refs = build_layer_style_refs(&layer).unwrap();
        assert_eq!(
            refs.style_refs_full,
            vec![
                "vejnavne__c0".to_string(),
                "vejnavne__c1".to_string(),
                "vejnavne__c2".to_string(),
                "vejnavne__major__label".to_string(),
                "vejnavne__other__label".to_string(),
                "vejnavne__label".to_string(),
            ]
        );
        assert_eq!(refs.class_label_indices, vec![Some(3), None, Some(4)]);
        assert_eq!(refs.layer_label_index, Some(5));
    }

    #[test]
    fn style_refs_layout_no_labels_omits_label_slots() {
        let layer = layer_with_classes("roads", &[Some("kind = 'main'"), None]);
        let refs = build_layer_style_refs(&layer).unwrap();
        assert_eq!(
            refs.style_refs_full,
            vec!["roads__c0".to_string(), "roads__c1".to_string()]
        );
        assert_eq!(refs.class_label_indices, vec![None, None]);
        assert_eq!(refs.layer_label_index, None);
    }

    #[test]
    fn style_refs_layout_only_layer_label_keeps_today_layout() {
        // pre-existing shape: classes have no labels, only the layer does.
        // the layer-label idx must still equal classes.len() (today's
        // invariant) so existing label sidecars decode unchanged.
        let mut layer = layer_with_classes("a", &[Some("k = '1'"), Some("k = '2'")]);
        layer.label = Some(label_plan("a__label"));
        let refs = build_layer_style_refs(&layer).unwrap();
        assert_eq!(refs.style_refs_full.len(), 3);
        assert_eq!(refs.class_label_indices, vec![None, None]);
        assert_eq!(refs.layer_label_index, Some(2));
    }
}

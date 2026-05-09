//! bootstrap snapshot: v3 substrate emission from a streamed source.
//!
//! C.2.b scope: per-binding multi-level page emission with class + label
//! sidecars. for each binding the snapshot:
//!   1. streams every row, decodes WKB -> FeatureGeom + decoded attrs;
//!   2. computes a Hilbert key per row over the combined bbox and sorts;
//!   3. for each configured decimation level: applies the level filters
//!      (geometry_min_size_m, vertex_tolerance_m), sweeps the kept rows
//!      into byte-budgeted pages, emits one page artifact per page;
//!   4. for each page * each layer that targets this binding: evaluates
//!      class assignments and label candidates, emits a class sidecar
//!      and (when the layer has labels) a label sidecar;
//!   5. writes a per-binding page-membership sidecar (sorted by feature_id,
//!      mmap'd) and folds it into the manifest atomically.
//!
//! out of scope here (C.2.c+):
//! - bucketed external-merge sort for bindings whose row set exceeds RAM
//!   (the LAZARUS plan calls for ~4 GiB working-set ceiling; in C.2.b we
//!   keep the in-memory path and document the limitation).
//! - rebuild from change-feed events (incremental.rs, sidecar lookups).
//! - label survival for features pruned out of a level (Independent
//!   policy currently emits labels for kept features only; pruned-feature
//!   anchors land alongside the rebuild path).

use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use futures_util::StreamExt;
use mars_artifact::{
    ArtifactKind, ArtifactWriter, AttrValue as ArtAttrValue, FeatureGeom, LabelCandidate, MAX_ROW_BYTES,
    SpatialIndexBuilder, compute_content_hash, encode_row, wkb_to_feature_geom,
};
use mars_source::{AttrValue, RowBytes, SourceBinding as PortBinding, SourceCollectionId};
use mars_store::ObjectStore;
use mars_types::{
    ArtifactEntry, ArtifactKey, Bbox, BindingMetadata, ContentHash, DecimationLevel, HilbertKey, LayerSidecarEntry,
    LayerSidecarKind, LevelMetadata, MANIFEST_FORMAT_VERSION, Manifest, PageEntry, PageId, PageKey,
};

use crate::class_eval::{LabelSpec, RowAttrs, assign_class, emit_label_candidate};
use crate::decimate::{passes_min_size, simplify};
use crate::external_sort::{ExternalSortConfig, WorkingSetGuard, bucketed_sort_in_place};
use crate::hilbert::key_from_centroid;
use crate::plan::{BindingPlan, BootstrapPlan, LayerPlan, LevelPlan};
use crate::sidecar::encode_sidecar;
use crate::{CompilerError, Deps};

/// Run a single snapshot pass against the bindings in `plan`. Writes every
/// page artifact + sidecar + manifest body via `deps`, returns the manifest
/// for the caller to publish.
pub async fn run_snapshot(
    deps: &Deps,
    plan: &BootstrapPlan,
    service_name: String,
    manifest_version: u64,
    working_set_bytes: u64,
) -> Result<Manifest, CompilerError> {
    let mut bindings_meta: Vec<BindingMetadata> = Vec::with_capacity(plan.bindings.len());
    let mut pages_meta: Vec<PageEntry> = Vec::new();
    let mut class_sidecars: Vec<LayerSidecarEntry> = Vec::new();
    let mut label_sidecars: Vec<LayerSidecarEntry> = Vec::new();

    for binding in &plan.bindings {
        let mut out = snapshot_one_binding(deps, binding, plan, working_set_bytes).await?;
        bindings_meta.push(out.meta);
        pages_meta.append(&mut out.pages);
        class_sidecars.append(&mut out.class_sidecars);
        label_sidecars.append(&mut out.label_sidecars);
    }

    pages_meta.sort_by(|a, b| {
        a.key
            .binding_id
            .as_str()
            .cmp(b.key.binding_id.as_str())
            .then_with(|| a.key.level.cmp(&b.key.level))
            .then_with(|| a.hilbert_range.0.cmp(&b.hilbert_range.0))
    });

    Ok(Manifest {
        format_version: MANIFEST_FORMAT_VERSION,
        version: manifest_version,
        service: service_name,
        created_at: SystemTime::now(),
        bindings: bindings_meta,
        pages: pages_meta,
        class_sidecars,
        label_sidecars,
        style_artifact: None,
        source_version: None,
        epoch: manifest_version,
    })
}

pub(crate) struct BindingOutput {
    pub(crate) meta: BindingMetadata,
    pub(crate) pages: Vec<PageEntry>,
    pub(crate) class_sidecars: Vec<LayerSidecarEntry>,
    pub(crate) label_sidecars: Vec<LayerSidecarEntry>,
}

pub(crate) async fn snapshot_one_binding(
    deps: &Deps,
    binding: &BindingPlan,
    plan: &BootstrapPlan,
    working_set_bytes: u64,
) -> Result<BindingOutput, CompilerError> {
    let rows = collect_binding_rows(deps, binding, working_set_bytes).await?;
    let total_features = rows.len() as u64;

    if rows.is_empty() {
        let meta = BindingMetadata {
            binding_id: binding.binding_id.clone(),
            source_table: binding.source_table.clone(),
            native_crs: binding.native_crs.clone(),
            feature_count_total: 0,
            levels: binding.levels.iter().map(empty_level_metadata).collect(),
            page_membership_sidecar: None,
        };
        return Ok(BindingOutput {
            meta,
            pages: Vec::new(),
            class_sidecars: Vec::new(),
            label_sidecars: Vec::new(),
        });
    }

    let combined_bbox = combined_bbox_of(&rows);

    let mut keyed: Vec<KeyedRow> = rows
        .into_iter()
        .map(|r| {
            let cx = (f64::from(r.feature.bbox[0]) + f64::from(r.feature.bbox[2])) / 2.0;
            let cy = (f64::from(r.feature.bbox[1]) + f64::from(r.feature.bbox[3])) / 2.0;
            let key = key_from_centroid(cx, cy, combined_bbox);
            KeyedRow { key, ..r }
        })
        .collect();
    bucketed_sort_in_place(&mut keyed, ExternalSortConfig::DEFAULT.bucket_bits, |r| r.key);

    // page-membership sidecar is computed once per binding (level-independent
    // mapping feature_id -> hilbert key).
    let mut sidecar_entries: Vec<(u64, HilbertKey)> = keyed.iter().map(|r| (r.feature.id, r.key)).collect();

    let layer_plans: Vec<&LayerPlan> = plan.layers_for(&binding.binding_id).collect();

    let mut all_pages: Vec<PageEntry> = Vec::new();
    let mut levels_meta: Vec<LevelMetadata> = Vec::with_capacity(binding.levels.len());
    let mut class_sidecars: Vec<LayerSidecarEntry> = Vec::new();
    let mut label_sidecars: Vec<LayerSidecarEntry> = Vec::new();

    for level in &binding.levels {
        let level_meta = emit_level(
            deps,
            binding,
            level,
            combined_bbox,
            &keyed,
            &layer_plans,
            &mut all_pages,
            &mut class_sidecars,
            &mut label_sidecars,
        )
        .await?;
        levels_meta.push(level_meta);
    }

    // page-membership sidecar.
    let sidecar_bytes = encode_sidecar(&mut sidecar_entries).map_err(|e| CompilerError::LegacySubstrateRetired {
        what: stringify_sidecar_err(&e),
    })?;
    let sidecar_hash = compute_content_hash(&sidecar_bytes);
    let sidecar_key = membership_sidecar_object_key(binding.binding_id.as_str(), &sidecar_hash)?;
    let sidecar_size = sidecar_bytes.len() as u64;
    deps.store.put(&sidecar_key, sidecar_bytes).await?;

    let meta = BindingMetadata {
        binding_id: binding.binding_id.clone(),
        source_table: binding.source_table.clone(),
        native_crs: binding.native_crs.clone(),
        feature_count_total: total_features,
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

#[allow(clippy::too_many_arguments)]
async fn emit_level(
    deps: &Deps,
    binding: &BindingPlan,
    level: &LevelPlan,
    combined_bbox: Bbox,
    keyed: &[KeyedRow],
    layers: &[&LayerPlan],
    out_pages: &mut Vec<PageEntry>,
    out_class_sidecars: &mut Vec<LayerSidecarEntry>,
    out_label_sidecars: &mut Vec<LayerSidecarEntry>,
) -> Result<LevelMetadata, CompilerError> {
    // pre-filter + simplify per the level's rules. retain Hilbert keys and
    // attrs without re-decoding.
    let mut leveled: Vec<KeyedRow> = Vec::with_capacity(keyed.len());
    for r in keyed {
        if !passes_min_size(&r.feature, level.geometry_min_size_m) {
            continue;
        }
        let geom = simplify(&r.feature.geom, level.vertex_tolerance_m, binding.simplifier);
        leveled.push(KeyedRow {
            feature: FeatureGeom {
                id: r.feature.id,
                bbox: r.feature.bbox,
                geom,
            },
            attrs: r.attrs.clone(),
            geom_bytes_estimate: r.geom_bytes_estimate,
            key: r.key,
        });
    }

    let mut pages_in_level: Vec<PageEntry> = Vec::new();
    let mut next_page_id: u64 = 0;
    let mut current: Vec<KeyedRow> = Vec::new();
    let mut current_bytes: u64 = 0;

    for r in leveled {
        let est = estimate_row_size(&r);
        if !current.is_empty() && current_bytes.saturating_add(est) > binding.page_size_target_bytes {
            let page = flush_page(deps, binding, level.level, PageId::new(next_page_id), &current).await?;
            emit_layer_sidecars(
                deps,
                level,
                &page,
                &current,
                layers,
                out_class_sidecars,
                out_label_sidecars,
            )
            .await?;
            pages_in_level.push(page);
            next_page_id += 1;
            current = Vec::new();
            current_bytes = 0;
        }
        current_bytes = current_bytes.saturating_add(est);
        current.push(r);
    }
    if !current.is_empty() {
        let page = flush_page(deps, binding, level.level, PageId::new(next_page_id), &current).await?;
        emit_layer_sidecars(
            deps,
            level,
            &page,
            &current,
            layers,
            out_class_sidecars,
            out_label_sidecars,
        )
        .await?;
        pages_in_level.push(page);
    }

    let level_meta = LevelMetadata {
        level: level.level,
        vertex_tolerance_m: level.vertex_tolerance_m,
        geometry_min_size_m: level.geometry_min_size_m,
        label_min_priority: level.label_min_priority,
        page_count: pages_in_level.len() as u32,
        combined_bbox,
        hilbert_range_table: pages_in_level.iter().map(|p| p.hilbert_range).collect(),
    };
    out_pages.append(&mut pages_in_level);
    Ok(level_meta)
}

/// One source row decoded into a feature, with attrs preserved for class /
/// label evaluation and a Hilbert key assigned over the binding's combined
/// bbox. Shared between the bootstrap and rebuild paths so both go through
/// the same `flush_page` / `emit_layer_sidecars` pipeline.
#[derive(Debug, Clone)]
pub(crate) struct KeyedRow {
    pub(crate) feature: FeatureGeom,
    pub(crate) attrs: Arc<Vec<(String, AttrValue)>>,
    pub(crate) geom_bytes_estimate: u64,
    pub(crate) key: HilbertKey,
}

async fn collect_binding_rows(
    deps: &Deps,
    binding: &BindingPlan,
    working_set_bytes: u64,
) -> Result<Vec<KeyedRow>, CompilerError> {
    let port_binding = PortBinding::new(
        SourceCollectionId::new(binding.binding_id.as_str()),
        binding_schema(&binding.source_table),
        binding_table(&binding.source_table),
        binding.geometry_column.clone(),
        binding.id_column.as_deref().unwrap_or("id"),
        binding.attributes.clone(),
        binding.native_crs.clone(),
    )?;
    let mut stream = deps.source.fetch_full_table_streaming(&port_binding).await?;

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
                binding: binding.binding_id.as_str().to_string(),
                observed_bytes: observed,
                ceiling_bytes: working_set_bytes,
            });
        }
        rows.push(KeyedRow {
            feature,
            attrs: Arc::new(row.attributes),
            geom_bytes_estimate,
            key: HilbertKey::min(),
        });
    }
    Ok(rows)
}

fn combined_bbox_of(rows: &[KeyedRow]) -> Bbox {
    let first = &rows[0].feature.bbox;
    let mut min_x = f64::from(first[0]);
    let mut min_y = f64::from(first[1]);
    let mut max_x = f64::from(first[2]);
    let mut max_y = f64::from(first[3]);
    for r in &rows[1..] {
        let bb = r.feature.bbox;
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
    }
    Bbox::new(min_x, min_y, max_x, max_y)
}

fn estimate_row_size(r: &KeyedRow) -> u64 {
    // approximate: WKB bytes (proxy for varint geom size) + attrs strings.
    let attr_bytes: usize = r.attrs.iter().map(|(k, _)| k.len() + 8).sum();
    r.geom_bytes_estimate + attr_bytes as u64 + 64
}

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

    let mut spatial_index = SpatialIndexBuilder::new(mars_artifact::DEFAULT_NODE_SIZE).map_err(|e| {
        CompilerError::LegacySubstrateRetired {
            what: stringify_artifact_err(&e),
        }
    })?;
    let mut features: Vec<FeatureGeom> = Vec::with_capacity(rows.len());
    let mut attrs_pairs: Vec<(u64, Vec<u8>)> = Vec::with_capacity(rows.len());

    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by_key(|&i| rows[i].feature.id);

    for (slot, &i) in order.iter().enumerate() {
        let r = &rows[i];
        let bb = r.feature.bbox;
        spatial_index.add(slot as u32, bb);
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
        let row_bytes = encode_row(&pairs).map_err(|e| CompilerError::LegacySubstrateRetired {
            what: stringify_attr_err(&e),
        })?;
        if row_bytes.len() > MAX_ROW_BYTES {
            return Err(CompilerError::LegacySubstrateRetired {
                what: "snapshot: row attributes exceed MAX_ROW_BYTES",
            });
        }
        attrs_pairs.push((r.feature.id, row_bytes.to_vec()));
    }

    let page_bbox = Bbox::new(min_x, min_y, max_x, max_y);
    let spatial_index_bytes = spatial_index
        .finish()
        .map_err(|e| CompilerError::LegacySubstrateRetired {
            what: stringify_artifact_err(&e),
        })?;

    let mut writer = ArtifactWriter::new(ArtifactKind::Source);
    writer
        .add_spatial_index(spatial_index_bytes)
        .add_geometry_payload(features)
        .add_attributes(attrs_pairs)
        .set_bbox(page_bbox)
        .set_feature_count(rows.len() as u64);
    let artifact_bytes: Bytes = writer.finish().map_err(|e| CompilerError::LegacySubstrateRetired {
        what: stringify_artifact_err(&e),
    })?;
    let hash = compute_content_hash(&artifact_bytes);

    let page_key = PageKey {
        binding_id: binding.binding_id.clone(),
        level,
        page_id,
    };
    let object_key = page_key
        .object_key(&hash)
        .map_err(|_| CompilerError::LegacySubstrateRetired {
            what: "snapshot: page key construction",
        })?;
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

pub(crate) async fn emit_layer_sidecars(
    deps: &Deps,
    level: &LevelPlan,
    page: &PageEntry,
    rows: &[KeyedRow],
    layers: &[&LayerPlan],
    out_class: &mut Vec<LayerSidecarEntry>,
    out_label: &mut Vec<LayerSidecarEntry>,
) -> Result<(), CompilerError> {
    // page-internal feature-id ordering follows flush_page (ascending by id).
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by_key(|&i| rows[i].feature.id);

    for layer in layers {
        let mut assignments: Vec<(u64, u16)> = Vec::with_capacity(rows.len());
        let mut labels: Vec<LabelCandidate> = Vec::new();

        let when_clauses: Vec<Option<mars_expr::Expr>> = layer.classes.iter().map(|c| c.when.clone()).collect();
        let style_refs: Vec<String> = layer.classes.iter().map(|c| c.style_ref.clone()).collect();
        let label_spec = layer.label.as_ref().map(|l| LabelSpec {
            priority: l.style.priority,
            text: &l.text,
            placement: &l.placement,
            // style_ref index = position in the (per-page) StyleRefs list.
            // class style_refs are emitted first; the label-style ref is
            // appended at the end so its index is `style_refs.len()`.
            style_ref_idx: u16::try_from(style_refs.len()).unwrap_or(u16::MAX),
        });

        for &i in &order {
            let r = &rows[i];
            let attrs = RowAttrs::new(r.attrs.as_ref());
            if let Some(idx) = assign_class(&when_clauses, &attrs) {
                assignments.push((r.feature.id, idx));
            }
            if let Some(spec) = &label_spec
                && let Some(c) = emit_label_candidate(
                    &r.feature,
                    &attrs,
                    spec,
                    layer.label_survival,
                    false, // geom_pruned == false here: features that didn't pass the level filter never reach the page.
                    level.label_min_priority,
                )
            {
                labels.push(c);
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
        let class_obj = class_entry
            .object_key()
            .map_err(|_| CompilerError::LegacySubstrateRetired {
                what: "snapshot: class sidecar key",
            })?;
        deps.store.put(&class_obj, class_bytes).await?;
        out_class.push(class_entry);

        if !labels.is_empty() {
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
            let label_obj = label_entry
                .object_key()
                .map_err(|_| CompilerError::LegacySubstrateRetired {
                    what: "snapshot: label sidecar key",
                })?;
            deps.store.put(&label_obj, label_bytes).await?;
            out_label.push(label_entry);
        }
    }
    Ok(())
}

fn build_class_artifact(
    assignments: &[(u64, u16)],
    style_refs: &[String],
    page_bbox: Bbox,
) -> Result<Bytes, CompilerError> {
    let mut writer = ArtifactWriter::new(ArtifactKind::Layer);
    writer
        .add_class_assignment(assignments)
        .add_style_refs(style_refs)
        .set_bbox(page_bbox)
        .set_feature_count(assignments.len() as u64);
    writer.finish().map_err(|e| CompilerError::LegacySubstrateRetired {
        what: stringify_artifact_err(&e),
    })
}

fn build_label_artifact(labels: &[LabelCandidate], page_bbox: Bbox) -> Result<Bytes, CompilerError> {
    let mut writer = ArtifactWriter::new(ArtifactKind::Layer);
    writer
        .add_label_candidates(labels)
        .set_bbox(page_bbox)
        .set_feature_count(labels.len() as u64);
    writer.finish().map_err(|e| CompilerError::LegacySubstrateRetired {
        what: stringify_artifact_err(&e),
    })
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
        return Err(CompilerError::LegacySubstrateRetired {
            what: "snapshot: sidecar key sanitisation",
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

// stringify_* helpers fold typed errors into LegacySubstrateRetired until the
// compiler error taxonomy is filled in alongside C.2.c rebuild paths.
pub(crate) fn stringify_wkb_err(_e: &mars_artifact::WkbError) -> &'static str {
    "snapshot: WKB decode"
}
fn stringify_attr_err(_e: &mars_artifact::AttrError) -> &'static str {
    "snapshot: attr encode"
}
fn stringify_artifact_err(_e: &mars_artifact::ArtifactError) -> &'static str {
    "snapshot: artifact assembly"
}
pub(crate) fn stringify_sidecar_err(_e: &crate::sidecar::SidecarError) -> &'static str {
    "snapshot: sidecar encode"
}

#[allow(dead_code)]
fn _arc_marker(_: Arc<dyn ObjectStore>) {}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::plan::{BindingPlan, BootstrapPlan, ClassPlan, LayerPlan, LevelPlan};
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures_core::stream::BoxStream;
    use futures_util::stream;
    use mars_artifact::{ArtifactReader, SectionKind, SpatialIndex, decode_class_assignment};
    use mars_observability::Metrics;
    use mars_source::{
        AttrValue, ChangeFeed, ChangeSubscription, LeaderLock, LeaderLockGuard, RowBytes, Source,
        SourceBinding as PortBinding, SourceError,
    };
    use mars_store::{ManifestStore, ObjectStore, StoreError};
    use mars_types::{ArtifactKey, BindingId, ContentHash, CrsCode, LayerId, Manifest};
    use std::sync::Mutex;

    #[derive(Default)]
    struct InMemoryStore {
        objects: Mutex<std::collections::HashMap<String, Bytes>>,
    }

    #[async_trait]
    impl ObjectStore for InMemoryStore {
        async fn get(&self, key: &ArtifactKey, _expected: ContentHash) -> Result<Bytes, StoreError> {
            self.objects
                .lock()
                .unwrap()
                .get(key.as_str())
                .cloned()
                .ok_or_else(|| StoreError::Transient(format!("missing {key}")))
        }
        async fn put(&self, key: &ArtifactKey, body: Bytes) -> Result<ContentHash, StoreError> {
            let hash = mars_artifact::compute_content_hash(&body);
            self.objects.lock().unwrap().insert(key.as_str().to_owned(), body);
            Ok(hash)
        }
        async fn delete(&self, _key: &ArtifactKey) -> Result<(), StoreError> {
            Ok(())
        }
        async fn list(&self, _prefix: &str) -> Result<Vec<ArtifactKey>, StoreError> {
            Ok(vec![])
        }
    }

    #[derive(Default)]
    struct PanicManifestStore;
    #[async_trait]
    impl ManifestStore for PanicManifestStore {
        async fn publish(&self, _manifest: &Manifest) -> Result<u64, StoreError> {
            panic!("publish should not be called from snapshot tests")
        }
        async fn current(&self) -> Result<Option<Manifest>, StoreError> {
            Ok(None)
        }
        async fn watch(
            &self,
        ) -> Result<futures_core::stream::BoxStream<'static, Result<Manifest, StoreError>>, StoreError> {
            Ok(Box::pin(stream::empty()))
        }
    }

    struct PointSource {
        rows: Vec<RowBytes>,
    }

    #[async_trait]
    impl Source for PointSource {
        async fn fetch_full_table_streaming<'a>(
            &'a self,
            _binding: &'a PortBinding,
        ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
            let owned: Vec<RowBytes> = self.rows.clone();
            Ok(Box::pin(stream::iter(owned.into_iter().map(Ok))))
        }

        async fn fetch_by_feature_ids<'a>(
            &'a self,
            _binding: &'a PortBinding,
            _ids: &'a [i64],
        ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
            Err(SourceError::NotImplemented {
                what: "test fetch_by_feature_ids",
            })
        }

        async fn stream_feature_ids<'a>(
            &'a self,
            _binding: &'a PortBinding,
        ) -> Result<BoxStream<'a, Result<i64, SourceError>>, SourceError> {
            Err(SourceError::NotImplemented {
                what: "test stream_feature_ids",
            })
        }
    }

    #[derive(Default)]
    struct NopChangeFeed;
    #[async_trait]
    impl ChangeFeed for NopChangeFeed {
        async fn subscribe(&self) -> Result<Box<dyn ChangeSubscription>, SourceError> {
            Err(SourceError::NotImplemented {
                what: "test ChangeFeed",
            })
        }
    }

    #[derive(Default)]
    struct NopLeaderLock;
    #[async_trait]
    impl LeaderLock for NopLeaderLock {
        async fn try_acquire(&self, _key: i64) -> Result<Option<Box<dyn LeaderLockGuard>>, SourceError> {
            Err(SourceError::NotImplemented {
                what: "test LeaderLock",
            })
        }
    }

    fn point_wkb(x: f64, y: f64) -> Bytes {
        let mut v = Vec::with_capacity(21);
        v.push(1);
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
        Bytes::from(v)
    }

    fn make_deps(rows: Vec<RowBytes>) -> (Deps, Arc<InMemoryStore>) {
        let store = Arc::new(InMemoryStore::default());
        let deps = Deps {
            source: Arc::new(PointSource { rows }),
            change_feed: Arc::new(NopChangeFeed),
            leader_lock: Arc::new(NopLeaderLock),
            store: store.clone(),
            manifest: Arc::new(PanicManifestStore),
            metrics: Metrics::new().unwrap(),
        };
        (deps, store)
    }

    fn binding_plan(id: &str, page_size: u64) -> BindingPlan {
        BindingPlan {
            binding_id: BindingId::try_new(id).unwrap(),
            source_table: id.to_string(),
            geometry_column: "geom".into(),
            id_column: Some("id".into()),
            attributes: vec!["name".into()],
            native_crs: CrsCode::new("EPSG:25832"),
            levels: vec![LevelPlan {
                level: DecimationLevel::new(0),
                vertex_tolerance_m: 0.0,
                geometry_min_size_m: 0.0,
                label_min_priority: 0,
            }],
            page_size_target_bytes: page_size,
            sidecar_size_warn_bytes: u64::MAX,
            reconcile_every_cycles: 24,
            simplifier: mars_config::SimplifierKind::Naive,
        }
    }

    fn layer_plan(layer: &str, binding: &str, with_label: bool) -> LayerPlan {
        let label = if with_label {
            Some(crate::plan::LayerLabelPlan {
                style_ref: format!("{layer}__label"),
                style: mars_style::LabelStyle {
                    font_family: "DejaVu Sans".into(),
                    font_size: 12.0,
                    fill: mars_style::Colour::rgb(0, 0, 0),
                    halo: None,
                    priority: 100,
                    min_distance: 0.0,
                },
                text: mars_expr::parse_template("{name}").unwrap(),
                placement: mars_style::Placement::Point,
            })
        } else {
            None
        };
        LayerPlan {
            layer_id: LayerId::new(layer),
            binding_id: BindingId::try_new(binding).unwrap(),
            kind: "point".into(),
            classes: vec![ClassPlan {
                name: "default".into(),
                when: None,
                style_ref: format!("{layer}__default"),
            }],
            label,
            label_survival: mars_style::LabelSurvival::Independent,
        }
    }

    #[tokio::test]
    async fn single_page_bootstrap_decodes_back_with_class_sidecar() {
        let rows: Vec<RowBytes> = (0..100)
            .map(|i| RowBytes {
                feature_id: i,
                geometry: point_wkb(f64::from(i as i32) * 10.0, f64::from(i as i32) * 5.0),
                attributes: vec![("name".into(), AttrValue::String(format!("p{i}")))],
            })
            .collect();
        let (deps, store) = make_deps(rows);
        let plan = BootstrapPlan {
            bindings: vec![binding_plan("points", 5 * 1024 * 1024)],
            layers: vec![layer_plan("dots", "points", true)],
        };

        let manifest = run_snapshot(&deps, &plan, "test".into(), 1, 4 * 1024 * 1024 * 1024).await.unwrap();
        assert_eq!(manifest.bindings.len(), 1);
        assert_eq!(manifest.bindings[0].feature_count_total, 100);
        assert_eq!(manifest.pages.len(), 1);
        assert_eq!(manifest.class_sidecars.len(), 1);
        assert_eq!(manifest.label_sidecars.len(), 1);

        let page = &manifest.pages[0];
        let key = page.key.object_key(&page.content_hash).unwrap();
        let bytes = store.objects.lock().unwrap().get(key.as_str()).unwrap().clone();
        let reader = ArtifactReader::open(bytes).unwrap();
        assert_eq!(reader.feature_count(), 100);
        let spix = SpatialIndex::open(reader.section(SectionKind::SpatialIndex).unwrap()).unwrap();
        let mut hits = Vec::new();
        spix.query([0.0, 0.0, 1000.0, 1000.0], &mut hits);
        assert!(!hits.is_empty());

        // class sidecar round-trips: every feature_id has a class assignment to index 0.
        let cls_entry = &manifest.class_sidecars[0];
        let cls_key = cls_entry.object_key().unwrap();
        let cls_bytes = store.objects.lock().unwrap().get(cls_key.as_str()).unwrap().clone();
        let cls_reader = ArtifactReader::open(cls_bytes).unwrap();
        let assignments =
            decode_class_assignment(&cls_reader.section(SectionKind::ClassAssignment).unwrap()).unwrap();
        assert_eq!(assignments.len(), 100);
        for (id, idx) in &assignments {
            assert_eq!(*idx, 0);
            assert!(*id < 100);
        }

        // label sidecar is keyed against the same page.
        let lbl_entry = &manifest.label_sidecars[0];
        assert_eq!(lbl_entry.page_key, cls_entry.page_key);
    }

    #[tokio::test]
    async fn three_levels_emit_independent_page_sets() {
        // 50 points on a diagonal, with extents [0..490, 0..490]. level 0 keeps
        // every feature; level 1 prunes the first 30 (they're all sub-30m points);
        // level 2 prunes everything (high min-size).
        let rows: Vec<RowBytes> = (0..50)
            .map(|i| RowBytes {
                feature_id: i,
                geometry: point_wkb(f64::from(i as i32) * 10.0, f64::from(i as i32) * 10.0),
                attributes: vec![("name".into(), AttrValue::String(format!("n{i}")))],
            })
            .collect();
        let (deps, _store) = make_deps(rows);

        // points have zero-area bbox so geometry_min_size_m always yields drop
        // when > 0; pick min_size such that level 1 keeps everything (0) and
        // level 2 drops everything (1.0) -- this just exercises the per-level
        // filter wiring.
        let bp = BindingPlan {
            binding_id: BindingId::try_new("points").unwrap(),
            source_table: "points".into(),
            geometry_column: "geom".into(),
            id_column: Some("id".into()),
            attributes: vec!["name".into()],
            native_crs: CrsCode::new("EPSG:25832"),
            levels: vec![
                LevelPlan {
                    level: DecimationLevel::new(0),
                    vertex_tolerance_m: 0.0,
                    geometry_min_size_m: 0.0,
                    label_min_priority: 0,
                },
                LevelPlan {
                    level: DecimationLevel::new(1),
                    vertex_tolerance_m: 0.0,
                    geometry_min_size_m: 0.0,
                    label_min_priority: 0,
                },
                LevelPlan {
                    level: DecimationLevel::new(2),
                    vertex_tolerance_m: 0.0,
                    geometry_min_size_m: 1.0,
                    label_min_priority: 0,
                },
            ],
            page_size_target_bytes: 5 * 1024 * 1024,
            sidecar_size_warn_bytes: u64::MAX,
            reconcile_every_cycles: 24,
            simplifier: mars_config::SimplifierKind::Naive,
        };
        let plan = BootstrapPlan {
            bindings: vec![bp],
            layers: vec![layer_plan("dots", "points", false)],
        };
        let manifest = run_snapshot(&deps, &plan, "test".into(), 1, 4 * 1024 * 1024 * 1024).await.unwrap();

        // levels 0 + 1 each have at least one page; level 2 has zero (all pruned).
        let level_pages = |lvl: u8| {
            manifest
                .pages
                .iter()
                .filter(|p| p.key.level.get() == lvl)
                .count()
        };
        assert!(level_pages(0) >= 1);
        assert!(level_pages(1) >= 1);
        assert_eq!(level_pages(2), 0);

        // bindings carry three level-metadata entries, one per configured level.
        assert_eq!(manifest.bindings[0].levels.len(), 3);
        assert_eq!(manifest.bindings[0].levels[2].page_count, 0);
    }

    #[tokio::test]
    async fn small_page_budget_splits_into_multiple_pages() {
        let rows: Vec<RowBytes> = (0..1000)
            .map(|i| RowBytes {
                feature_id: i,
                geometry: point_wkb(f64::from(i as i32), f64::from((i * 7) as i32)),
                attributes: vec![("name".into(), AttrValue::String(format!("x{i}")))],
            })
            .collect();
        let (deps, _store) = make_deps(rows);
        let plan = BootstrapPlan {
            bindings: vec![binding_plan("pts", 16 * 1024)],
            layers: vec![],
        };
        let manifest = run_snapshot(&deps, &plan, "test".into(), 1, 4 * 1024 * 1024 * 1024).await.unwrap();
        let pages: Vec<&PageEntry> = manifest.pages.iter().collect();
        assert!(pages.len() > 1);
        let total: u64 = pages.iter().map(|p| p.feature_count).sum();
        assert_eq!(total, 1000);
        let level_table = &manifest.bindings[0].levels[0].hilbert_range_table;
        for w in level_table.windows(2) {
            assert!(w[0].1 <= w[1].0, "overlapping or out-of-order ranges: {w:?}");
        }
        // no layers in plan -> no class/label sidecars.
        assert!(manifest.class_sidecars.is_empty());
        assert!(manifest.label_sidecars.is_empty());
    }

    #[tokio::test]
    async fn working_set_ceiling_yields_named_error() {
        // 64-byte ceiling is below the per-row floor of 64 + attr_bytes + WKB,
        // so the second row puts us over.
        let rows: Vec<RowBytes> = (0..8)
            .map(|i| RowBytes {
                feature_id: i,
                geometry: point_wkb(f64::from(i as i32), f64::from(i as i32)),
                attributes: vec![("name".into(), AttrValue::String(format!("row{i}")))],
            })
            .collect();
        let (deps, _store) = make_deps(rows);
        let plan = BootstrapPlan {
            bindings: vec![binding_plan("pts", 16 * 1024)],
            layers: vec![],
        };
        let err = run_snapshot(&deps, &plan, "test".into(), 1, 64).await.unwrap_err();
        match err {
            CompilerError::WorkingSetExceeded {
                binding,
                observed_bytes,
                ceiling_bytes,
            } => {
                assert_eq!(binding, "pts");
                assert!(observed_bytes > ceiling_bytes);
                assert_eq!(ceiling_bytes, 64);
            }
            other => panic!("expected WorkingSetExceeded, got {other:?}"),
        }
    }
}

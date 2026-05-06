//! Execute a [`Plan`]: per source-cell, fetch rows once, materialise the
//! shared source artifact, then derive each dependent layer artifact.

use std::sync::Arc;

use bytes::Bytes;
use mars_artifact::{ArtifactKind, ArtifactWriter, SourceRef, compute_content_hash};
use mars_source::{RowAttrs, RowBytes, Source};
use mars_store::ObjectStore;
use mars_types::{ArtifactEntry, ArtifactKey, Bbox, Cell, ContentHash, EmptyLayerCell};

use crate::CompilerError;
use crate::class::{CompiledClass, first_match};
use crate::plan::{LayerTask, SourceTask, cell_bbox};

#[derive(Debug, Default)]
pub struct SnapshotOutput {
    pub source_artifacts: Vec<ArtifactEntry>,
    pub layer_artifacts: Vec<ArtifactEntry>,
    pub empty_layer_cells: Vec<EmptyLayerCell>,
}

impl SnapshotOutput {
    pub fn extend(&mut self, other: SnapshotOutput) {
        self.source_artifacts.extend(other.source_artifacts);
        self.layer_artifacts.extend(other.layer_artifacts);
        self.empty_layer_cells.extend(other.empty_layer_cells);
    }
}

/// Build one source cell and every layer artifact that depends on it.
///
/// `dependents` is the slice of layer tasks whose `source` index points at
/// `task`. The shared rows are fetched once; classification + layer-artifact
/// emission iterate in memory.
pub async fn run_source_cell(
    task: &Arc<SourceTask>,
    dependents: &[Arc<LayerTask>],
    source: &Arc<dyn Source>,
    store: &Arc<dyn ObjectStore>,
) -> Result<SnapshotOutput, CompilerError> {
    let mut out = SnapshotOutput::default();
    let bbox = cell_bbox(task.origin, task.cell_size, &task.cell);
    let rows = source.fetch_cell(&task.binding, &task.cell, bbox, None).await?;
    if rows.is_empty() {
        for dep in dependents {
            out.empty_layer_cells.push(EmptyLayerCell {
                layer: dep.layer.clone(),
                cell: Cell {
                    band: dep.band.clone(),
                    x: dep.cell.x,
                    y: dep.cell.y,
                },
            });
        }
        return Ok(out);
    }
    let mut rows = rows;
    rows.sort_by_key(|r| r.feature_id);

    let task_blocking = task.clone();
    let deps_blocking: Vec<Arc<LayerTask>> = dependents.to_vec();
    let (src_entry, src_bytes, layer_outputs) = tokio::task::spawn_blocking(move || {
        let (src_entry, src_bytes) = build_source_artifact(&task_blocking, &rows)?;
        let collection = task_blocking.collection().as_str();
        let mut layer_outputs: Vec<(ArtifactEntry, Bytes)> = Vec::with_capacity(deps_blocking.len());
        for dep in &deps_blocking {
            let (entry, bytes) = build_layer_artifact(dep, &rows, src_entry.hash, bbox, collection)?;
            layer_outputs.push((entry, bytes));
        }
        Ok::<_, CompilerError>((src_entry, src_bytes, layer_outputs))
    })
    .await
    .map_err(|e| CompilerError::BuildTaskPanic { reason: e.to_string() })??;

    store.put(&src_entry.key, src_bytes).await?;
    out.source_artifacts.push(src_entry);

    for (entry, bytes) in layer_outputs {
        store.put(&entry.key, bytes).await?;
        out.layer_artifacts.push(entry);
    }
    Ok(out)
}

fn build_source_artifact(task: &SourceTask, rows: &[RowBytes]) -> Result<(ArtifactEntry, Bytes), CompilerError> {
    let expected_srid = task
        .binding
        .crs
        .as_str()
        .strip_prefix("EPSG:")
        .and_then(|s| s.parse::<u32>().ok());
    let mut features = Vec::with_capacity(rows.len());
    let mut acc = BboxAcc::new();
    for row in rows {
        let f = crate::wkb::decode_feature(row.feature_id, &row.geometry, expected_srid)?;
        acc.fold(f.bbox);
        features.push(f);
    }
    let mut writer = ArtifactWriter::new(ArtifactKind::Source);
    let feature_count = features.len() as u64;
    writer.add_geometry_payload(features);
    writer.set_bbox(acc.into_bbox());
    writer.set_feature_count(feature_count);
    let bytes = writer.finish()?;
    let hash = compute_content_hash(&bytes);
    let key = ArtifactKey::try_build_source(task.collection().as_str(), &task.cell, hash)
        .map_err(|e| crate::plan::PlanError::Invalid(e.to_string()))?;
    let entry = ArtifactEntry {
        key,
        hash,
        size_bytes: bytes.len() as u64,
    };
    Ok((entry, bytes))
}

fn build_layer_artifact(
    task: &LayerTask,
    rows: &[RowBytes],
    source_hash: ContentHash,
    bbox: Bbox,
    source_collection: &str,
) -> Result<(ArtifactEntry, Bytes), CompilerError> {
    let mut assignments: Vec<(u64, u16)> = Vec::with_capacity(rows.len());
    for row in rows {
        let attrs = RowAttrs::new(&row.attributes);
        if let Some(idx) = first_match(&task.classes, &attrs)? {
            assignments.push((row.feature_id, idx));
        }
    }
    let style_refs: Vec<String> = task
        .classes
        .iter()
        .map(|c: &CompiledClass| c.style_id.clone())
        .collect();

    let mut writer = ArtifactWriter::new(ArtifactKind::Layer);
    writer.add_class_assignment(&assignments);
    writer.add_style_refs(&style_refs);
    writer.set_bbox(bbox);
    writer.set_feature_count(assignments.len() as u64);
    writer.set_source_ref(SourceRef {
        collection: source_collection.to_string(),
        band: task.band.as_str().to_string(),
        cell_x: task.cell.x,
        cell_y: task.cell.y,
        content_hash: source_hash,
    });
    let bytes = writer.finish()?;
    let hash = compute_content_hash(&bytes);
    let key = ArtifactKey::try_build_layer(&task.layer, &task.cell, hash)
        .map_err(|e| crate::plan::PlanError::Invalid(e.to_string()))?;
    let entry = ArtifactEntry {
        key,
        hash,
        size_bytes: bytes.len() as u64,
    };
    Ok((entry, bytes))
}

struct BboxAcc {
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
    seen: bool,
}
impl BboxAcc {
    fn new() -> Self {
        Self {
            min_x: f32::INFINITY,
            min_y: f32::INFINITY,
            max_x: f32::NEG_INFINITY,
            max_y: f32::NEG_INFINITY,
            seen: false,
        }
    }
    fn fold(&mut self, b: [f32; 4]) {
        self.seen = true;
        if b[0] < self.min_x {
            self.min_x = b[0];
        }
        if b[1] < self.min_y {
            self.min_y = b[1];
        }
        if b[2] > self.max_x {
            self.max_x = b[2];
        }
        if b[3] > self.max_y {
            self.max_y = b[3];
        }
    }
    fn into_bbox(self) -> Bbox {
        if !self.seen {
            return Bbox::new(0.0, 0.0, 0.0, 0.0);
        }
        Bbox::new(
            self.min_x as f64,
            self.min_y as f64,
            self.max_x as f64,
            self.max_y as f64,
        )
    }
}

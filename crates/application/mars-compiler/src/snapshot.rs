//! execute one BuildTask: per-cell fetch -> source artifact -> layer artifact ->
//! manifest entries. snapshot path only (SPEC §8.2.3).

use std::sync::Arc;

use bytes::Bytes;
use mars_artifact::{ArtifactKind, ArtifactWriter, SourceRef, compute_content_hash};
use mars_source::{RowAttrs, RowBytes, Source};
use mars_store::ObjectStore;
use mars_types::{ArtifactEntry, ArtifactKey, Bbox, Cell, ContentHash, EmptyLayerCell, LayerId, ScaleBand};

use crate::CompilerError;
use crate::class::{CompiledClass, first_match};
use crate::plan::{BuildTask, cell_bbox};
use crate::wkb;

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

/// Run a single cell of a build task. Cells are the unit of parallelism;
/// the driver fans these out via `buffer_unordered`.
pub async fn run_cell(
    task: &Arc<BuildTask>,
    cell: &Cell,
    source: &Arc<dyn Source>,
    store: &Arc<dyn ObjectStore>,
) -> Result<SnapshotOutput, CompilerError> {
    let mut out = SnapshotOutput::default();
    let bbox = cell_bbox(task.origin, task.cell_size, cell);
    let rows = source.fetch_cell(&task.binding, cell, bbox, None).await?;
    if rows.is_empty() {
        out.empty_layer_cells.push(EmptyLayerCell {
            layer: task.layer.clone(),
            cell: Cell {
                band: task.band.clone(),
                x: cell.x,
                y: cell.y,
            },
        });
        return Ok(out);
    }
    // sort by feature id for deterministic layout
    let mut rows = rows;
    rows.sort_by_key(|r| r.feature_id);

    let task_for_blocking = task.clone();
    let cell_for_blocking = cell.clone();
    let (src_entry, src_bytes, layer_entry, layer_bytes) = tokio::task::spawn_blocking(move || {
        let (src_entry, src_bytes) = build_source_artifact(&task_for_blocking, &cell_for_blocking, &rows)?;
        let (layer_entry, layer_bytes) =
            build_layer_artifact(&task_for_blocking, &cell_for_blocking, &rows, src_entry.hash, bbox)?;
        Ok::<_, CompilerError>((src_entry, src_bytes, layer_entry, layer_bytes))
    })
    .await
    .map_err(|e| CompilerError::BuildTaskPanic { reason: e.to_string() })??;

    store.put(&src_entry.key, src_bytes).await?;
    out.source_artifacts.push(src_entry);

    store.put(&layer_entry.key, layer_bytes).await?;
    out.layer_artifacts.push(layer_entry);
    Ok(out)
}

fn build_source_artifact(
    task: &BuildTask,
    cell: &Cell,
    rows: &[RowBytes],
) -> Result<(ArtifactEntry, Bytes), CompilerError> {
    let expected_srid = task
        .binding
        .crs
        .as_str()
        .strip_prefix("EPSG:")
        .and_then(|s| s.parse::<u32>().ok());
    let mut features = Vec::with_capacity(rows.len());
    let mut acc = BboxAcc::new();
    for row in rows {
        let f = wkb::decode_feature(row.feature_id, &row.geometry, expected_srid)?;
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
    let cell_with_band = Cell {
        band: ScaleBand::new(task.band.as_str()),
        x: cell.x,
        y: cell.y,
    };
    let key = ArtifactKey::try_build_source(task.binding.collection.as_str(), &cell_with_band, hash)
        .map_err(|e| crate::plan::PlanError::Invalid(e.to_string()))?;
    let entry = ArtifactEntry {
        key,
        hash,
        size_bytes: bytes.len() as u64,
    };
    Ok((entry, bytes))
}

fn build_layer_artifact(
    task: &BuildTask,
    cell: &Cell,
    rows: &[RowBytes],
    source_hash: ContentHash,
    bbox: Bbox,
) -> Result<(ArtifactEntry, Bytes), CompilerError> {
    let mut assignments: Vec<(u64, u16)> = Vec::with_capacity(rows.len());
    for row in rows {
        let attrs = RowAttrs::new(&row.attributes);
        if let Some(idx) = first_match(&task.classes, &attrs)? {
            assignments.push((row.feature_id, idx));
        }
    }
    // already sorted: rows came in id order
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
        collection: task.binding.collection.as_str().to_string(),
        band: task.band.as_str().to_string(),
        cell_x: cell.x,
        cell_y: cell.y,
        content_hash: source_hash,
    });
    let bytes = writer.finish()?;
    let hash = compute_content_hash(&bytes);
    let layer_id = LayerId::new(task.layer.as_str());
    let cell_with_band = Cell {
        band: ScaleBand::new(task.band.as_str()),
        x: cell.x,
        y: cell.y,
    };
    let key = ArtifactKey::try_build_layer(&layer_id, &cell_with_band, hash)
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

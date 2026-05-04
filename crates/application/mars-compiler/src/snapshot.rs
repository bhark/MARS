//! execute one BuildTask: per-cell fetch -> source artifact -> layer artifact ->
//! manifest entries. snapshot path only (SPEC §8.2.3).

use std::sync::Arc;

use mars_artifact::{ArtifactKind, ArtifactWriter, SourceRef, compute_content_hash};
use mars_source::{RowAttrs, RowBytes, Source};
use mars_store::ObjectStore;
use mars_types::{ArtifactEntry, ArtifactKey, Bbox, ContentHash};

use crate::CompilerError;
use crate::class::{CompiledClass, first_match};
use crate::plan::{BuildTask, cell_bbox};
use crate::wkb;

/// `schema_version` lives only on layer artifacts (SPEC §9.2). Bumped when the
/// layer artifact's payload format changes.
pub const LAYER_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Default)]
pub struct SnapshotOutput {
    pub source_artifacts: Vec<ArtifactEntry>,
    pub layer_artifacts: Vec<ArtifactEntry>,
}

impl SnapshotOutput {
    pub fn extend(&mut self, other: SnapshotOutput) {
        self.source_artifacts.extend(other.source_artifacts);
        self.layer_artifacts.extend(other.layer_artifacts);
    }
}

pub async fn run_task(
    task: &BuildTask,
    source: &Arc<dyn Source>,
    store: &Arc<dyn ObjectStore>,
) -> Result<SnapshotOutput, CompilerError> {
    let mut out = SnapshotOutput::default();
    for cell in &task.cells {
        let bbox = cell_bbox(task.origin, task.cell_size, cell);
        let rows = source
            .fetch_cell(&task.binding, cell, bbox, None)
            .await?;
        if rows.is_empty() {
            continue;
        }
        // sort by feature id for deterministic layout
        let mut rows = rows;
        rows.sort_by_key(|r| r.feature_id);

        let (src_entry, src_hash) = build_source_artifact(task, cell, &rows, store).await?;
        out.source_artifacts.push(src_entry);

        let layer_entry =
            build_layer_artifact(task, cell, &rows, src_hash, store).await?;
        out.layer_artifacts.push(layer_entry);
    }
    Ok(out)
}

async fn build_source_artifact(
    task: &BuildTask,
    cell: &mars_types::Cell,
    rows: &[RowBytes],
    store: &Arc<dyn ObjectStore>,
) -> Result<(ArtifactEntry, ContentHash), CompilerError> {
    let mut features = Vec::with_capacity(rows.len());
    let mut acc = BboxAcc::new();
    for row in rows {
        let f = wkb::decode_feature(row.feature_id, &row.geometry)
            .map_err(|e| CompilerError::Wkb(e.to_string()))?;
        acc.fold(f.bbox);
        features.push(f);
    }
    let mut writer = ArtifactWriter::new(ArtifactKind::Source);
    writer
        .add_geometry_payload(&features)
        .map_err(|e| CompilerError::Artifact(e.to_string()))?;
    writer.set_bbox(acc.into_bbox());
    writer.set_feature_count(features.len() as u64);
    let bytes = writer
        .finish()
        .map_err(|e| CompilerError::Artifact(e.to_string()))?;
    let hash = compute_content_hash(&bytes);
    let key = ArtifactKey::new(format!(
        "src/{coll}/{band}/{cx}_{cy}/{hex}.mars",
        coll = task.binding.collection.as_str(),
        band = task.band.as_str(),
        cx = cell.x,
        cy = cell.y,
        hex = hex32(&hash.0),
    ));
    let size = bytes.len() as u64;
    let written = store.put(&key, bytes).await?;
    debug_assert_eq!(written, hash);
    Ok((
        ArtifactEntry {
            key,
            hash,
            size_bytes: size,
        },
        hash,
    ))
}

async fn build_layer_artifact(
    task: &BuildTask,
    cell: &mars_types::Cell,
    rows: &[RowBytes],
    source_hash: ContentHash,
    store: &Arc<dyn ObjectStore>,
) -> Result<ArtifactEntry, CompilerError> {
    let mut assignments: Vec<(u64, u16)> = Vec::with_capacity(rows.len());
    for row in rows {
        let attrs = RowAttrs::new(&row.attributes);
        if let Some(idx) = first_match(&task.classes, &attrs)
            .map_err(|e| CompilerError::Expr(e.to_string()))?
        {
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
    writer.set_feature_count(assignments.len() as u64);
    writer.set_source_ref(SourceRef {
        collection: task.binding.collection.as_str().to_string(),
        band: task.band.as_str().to_string(),
        cell_x: cell.x,
        cell_y: cell.y,
        content_hash: source_hash,
    });
    let bytes = writer
        .finish()
        .map_err(|e| CompilerError::Artifact(e.to_string()))?;
    let hash = compute_content_hash(&bytes);
    let key = ArtifactKey::new(format!(
        "lyr/{layer}/{band}/{cx}_{cy}/v{schema}/{hex}.mars",
        layer = task.layer.as_str(),
        band = task.band.as_str(),
        cx = cell.x,
        cy = cell.y,
        schema = LAYER_SCHEMA_VERSION,
        hex = hex32(&hash.0),
    ));
    let size = bytes.len() as u64;
    let written = store.put(&key, bytes).await?;
    debug_assert_eq!(written, hash);
    Ok(ArtifactEntry {
        key,
        hash,
        size_bytes: size,
    })
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

fn hex32(b: &[u8; 32]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(64);
    for byte in b {
        // infallible: pre-allocated string
        let _ = write!(s, "{byte:02x}");
    }
    s
}

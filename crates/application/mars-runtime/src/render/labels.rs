//! label pipeline: candidate decode, projection, collision, emission.
//!
//! self-contained slice of the render path. `prepare_labels` opens a label
//! sidecar and produces `PreparedLabel`s already projected into request-CRS
//! pixel space; `collide_and_emit_labels` runs the priority-ordered greedy
//! collision pass over the union of all layers' candidates and emits the
//! surviving `DrawOp::Label` ops in placement order.

use std::sync::Arc;

use bytes::Bytes;
use mars_artifact::{ArtifactReader, LabelCandidate, LabelShape, SectionKind, decode_label_candidates};
use mars_render_port::{DrawOp, Renderer};
use mars_style::{LabelStyle, Stylesheet};
use mars_types::BindingMetadata;

use crate::{RenderPlan, RuntimeError};

use super::decode::ClassResolver;
use super::project::world_to_pixel;
use super::{map_artifact_err, map_proj_err};

/// label candidate that has been resolved against the active stylesheet and
/// projected into request-CRS pixel space. carries enough state for the
/// collision pass to keep or drop it without redoing the projection.
pub(super) struct PreparedLabel {
    anchor_px: (f32, f32),
    text: String,
    style: Arc<LabelStyle>,
    priority: u16,
    bbox_px: (f32, f32, f32, f32),
    /// counter-clockwise rotation in radians; non-zero for line labels
    /// sampled along a polyline.
    angle_rad: f32,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_labels(
    bytes: Bytes,
    plan: &RenderPlan,
    binding: &BindingMetadata,
    class: Option<&ClassResolver>,
    stylesheet: &Stylesheet,
    same_crs: bool,
    survival_filter: Option<&[bool]>,
    renderer: &dyn Renderer,
) -> Result<Vec<PreparedLabel>, RuntimeError> {
    let reader = ArtifactReader::open(bytes).map_err(map_artifact_err)?;
    let label_bytes = reader.section(SectionKind::LabelCandidates).map_err(map_artifact_err)?;
    let candidates = decode_label_candidates(&label_bytes).map_err(map_artifact_err)?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(candidates.len());
    let xform = if same_crs {
        None
    } else {
        Some(mars_proj::cached_transformer(&binding.native_crs, &plan.crs).map_err(map_proj_err)?)
    };
    for c in candidates {
        // FollowGeometry: drop slot-bearing candidates whose feature wasn't
        // rendered at this scale. slotless (pruned-feature) labels are
        // emitted unconditionally - they exist precisely because their
        // geometry was filtered out at compile time. compiler is the
        // primary enforcer; runtime stays defensive against drift (eg. an
        // older sidecar epoch left over after a swap).
        if let (Some(allow), Some(idx)) = (survival_filter, c.feature_idx) {
            let i = idx as usize;
            if i >= allow.len() || !allow[i] {
                continue;
            }
        }
        let anchor_world = match label_anchor_world(&c, xform.as_deref()) {
            Some(a) => a,
            None => continue,
        };
        let anchor_px = world_to_pixel(anchor_world, plan.bbox, plan.width, plan.height);
        if !inside_pixel_canvas(anchor_px, plan.width, plan.height) {
            continue;
        }
        let style_name = class
            .and_then(|cl| cl.style_refs().get(c.style_ref_idx as usize))
            .map(String::as_str);
        let Some(style) = style_name.and_then(|n| stylesheet.labels.get(n).cloned()) else {
            continue;
        };
        let bbox_px = match renderer.measure_text(&c.text, &style) {
            Ok(m) => text_bbox_from_metrics(anchor_px, m),
            // font lookup / shaping failure: drop the candidate. matches the
            // existing "drop on error" behaviour of style/anchor resolution
            // a few lines above.
            Err(_) => continue,
        };
        out.push(PreparedLabel {
            anchor_px,
            text: c.text,
            style,
            priority: c.priority,
            bbox_px,
            angle_rad: 0.0,
        });
    }
    Ok(out)
}

fn label_anchor_world(c: &LabelCandidate, xform: Option<&mars_proj::Transformer>) -> Option<(f64, f64)> {
    let (wx, wy) = match &c.shape {
        LabelShape::Point { x, y } | LabelShape::PolygonAnchor { x, y } => (f64::from(*x), f64::from(*y)),
        // polyline labels: take the midpoint of the polyline. naive but
        // reasonable for v1; a future pass that reuses the placement
        // engine's arc-length sampling can refine.
        LabelShape::Polyline(points) => {
            if points.is_empty() {
                return None;
            }
            let mid = points[points.len() / 2];
            (f64::from(mid.0), f64::from(mid.1))
        }
    };
    match xform {
        None => Some((wx, wy)),
        Some(t) => t.transform_point(wx, wy).ok(),
    }
}

fn inside_pixel_canvas(p: (f32, f32), w: u32, h: u32) -> bool {
    p.0 >= 0.0 && p.1 >= 0.0 && p.0 <= w as f32 && p.1 <= h as f32
}

/// build a pixel-space bbox around `anchor` from the font-aware metrics the
/// renderer would use to rasterise the same run. anchor is the baseline
/// origin; bbox extends by half advance horizontally and by ascent / descent
/// vertically. centred horizontally because draw_label paints around the
/// anchor; the vertical extent uses the actual font ascent + descent so the
/// collision bbox matches what tiny-skia paints.
fn text_bbox_from_metrics(anchor: (f32, f32), m: mars_render_port::TextMetrics) -> (f32, f32, f32, f32) {
    let half_w = m.advance_x * 0.5;
    (
        anchor.0 - half_w,
        anchor.1 - m.ascent,
        anchor.0 + half_w,
        anchor.1 + m.descent,
    )
}

/// run a greedy collision pass over the accumulated label set and return
/// the surviving `DrawOp::Label` ops in placement order.
pub(super) fn collide_and_emit_labels(mut labels: Vec<PreparedLabel>, _w: u32, _h: u32) -> Vec<DrawOp> {
    if labels.is_empty() {
        return Vec::new();
    }
    // priority desc → place high-priority labels first, drop conflicts.
    labels.sort_by_key(|l| std::cmp::Reverse(l.priority));
    let mut placed: Vec<(f32, f32, f32, f32)> = Vec::with_capacity(labels.len());
    let mut ops = Vec::with_capacity(labels.len());
    for label in labels {
        if placed.iter().any(|b| pixel_bbox_overlaps(*b, label.bbox_px)) {
            continue;
        }
        placed.push(label.bbox_px);
        ops.push(DrawOp::Label {
            anchor: label.anchor_px,
            text: label.text,
            style: label.style,
            angle_rad: label.angle_rad,
        });
    }
    ops
}

fn pixel_bbox_overlaps(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    a.0 < b.2 && a.2 > b.0 && a.1 < b.3 && a.3 > b.1
}

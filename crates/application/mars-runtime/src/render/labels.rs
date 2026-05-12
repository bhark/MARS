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
use mars_style::{LabelStyle, Placement, Stylesheet};
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
    placement: &Placement,
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
        let style_name = class
            .and_then(|cl| cl.style_refs().get(c.style_ref_idx as usize))
            .map(String::as_str);
        let Some(style) = style_name.and_then(|n| stylesheet.labels.get(n).cloned()) else {
            continue;
        };
        // polyline candidates expand into multiple samples along the line
        // when the layer placement is Placement::Line; otherwise fall through
        // to the historical midpoint anchor.
        if let (
            LabelShape::Polyline(points),
            Placement::Line {
                repeat_m,
                max_angle_delta_deg,
            },
        ) = (&c.shape, placement)
        {
            sample_polyline_labels(
                points,
                xform.as_deref(),
                plan,
                *repeat_m,
                *max_angle_delta_deg,
                &c.text,
                c.priority,
                &style,
                renderer,
                &mut out,
            );
            continue;
        }
        let anchor_world = match label_anchor_world(&c, xform.as_deref()) {
            Some(a) => a,
            None => continue,
        };
        let anchor_px = world_to_pixel(anchor_world, plan.bbox, plan.width, plan.height);
        if !inside_pixel_canvas(anchor_px, plan.width, plan.height) {
            continue;
        }
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

/// project the polyline to pixel space, walk it at `repeat_m` intervals
/// (converted to pixels via the plan's pixel size), and emit one
/// `PreparedLabel` per accepted sample. each sample carries the pixel-space
/// tangent angle and a rotated bbox for the collision pass.
#[allow(clippy::too_many_arguments)]
fn sample_polyline_labels(
    points: &[(f32, f32)],
    xform: Option<&mars_proj::Transformer>,
    plan: &RenderPlan,
    repeat_m: f64,
    max_angle_delta_deg: f32,
    text: &str,
    priority: u16,
    style: &Arc<LabelStyle>,
    renderer: &dyn Renderer,
    out: &mut Vec<PreparedLabel>,
) {
    if points.len() < 2 {
        return;
    }
    let pixel_pts = match project_polyline_to_pixels(points, xform, plan) {
        Some(p) => p,
        None => return,
    };
    let cum = cumulative_arc_length(&pixel_pts);
    let arc_total = match cum.last().copied() {
        Some(t) if t > 0.0 => t,
        _ => return,
    };
    // sample spacing in pixels: repeat_m is in source-CRS units (metres in
    // projected CRSs); convert via the plan's standardised m/px. fall back
    // to source units when scale_pixel_size_m is degenerate so we still
    // produce at least one sample.
    let repeat_px = if plan.scale_pixel_size_m.is_finite() && plan.scale_pixel_size_m > 0.0 {
        (repeat_m / plan.scale_pixel_size_m) as f32
    } else {
        repeat_m as f32
    };
    if !(repeat_px.is_finite() && repeat_px > 1.0) {
        return;
    }

    let metrics = match renderer.measure_text(text, style) {
        Ok(m) => m,
        Err(_) => return,
    };
    let half_advance = metrics.advance_x * 0.5;
    let half_h = metrics.ascent.max(metrics.descent);
    if metrics.advance_x <= 0.0 {
        return;
    }
    let max_delta = max_angle_delta_deg.to_radians();

    let n = ((arc_total / repeat_px).floor() as i32).max(1);
    let step = arc_total / n as f32;
    // centred placement: positions at (i + 0.5) * step. matches the existing
    // midpoint behaviour when n_samples = 1.
    for i in 0..n {
        let pos = (i as f32 + 0.5) * step;
        let Some(centre) = sample_at(&pixel_pts, &cum, pos) else {
            continue;
        };
        // bail when the label footprint runs past either end of the line; no
        // attempt at clipping the text, just skip the candidate.
        if pos < half_advance || pos + half_advance > arc_total {
            continue;
        }
        let Some(head) = sample_at(&pixel_pts, &cum, pos + half_advance) else {
            continue;
        };
        let Some(tail) = sample_at(&pixel_pts, &cum, pos - half_advance) else {
            continue;
        };
        // angle gate: reject candidates whose tangent rotates too much
        // across the label footprint.
        let angle_head = atan2_signed(head.tangent);
        let angle_tail = atan2_signed(tail.tangent);
        if angle_diff(angle_head, angle_tail).abs() > max_delta {
            continue;
        }
        let mut angle = atan2_signed(centre.tangent);
        // keep text reading left-to-right: flip the tangent by pi when the
        // sample tangent points into the left half-plane.
        if !(-std::f32::consts::FRAC_PI_2..=std::f32::consts::FRAC_PI_2).contains(&angle) {
            angle += std::f32::consts::PI;
            if angle > std::f32::consts::PI {
                angle -= std::f32::consts::TAU;
            }
        }
        if !inside_pixel_canvas(centre.pos, plan.width, plan.height) {
            continue;
        }
        let bbox_px = rotated_label_bbox(centre.pos, half_advance, half_h, angle);
        out.push(PreparedLabel {
            anchor_px: centre.pos,
            text: text.to_owned(),
            style: style.clone(),
            priority,
            bbox_px,
            angle_rad: angle,
        });
    }
}

fn project_polyline_to_pixels(
    points: &[(f32, f32)],
    xform: Option<&mars_proj::Transformer>,
    plan: &RenderPlan,
) -> Option<Vec<(f32, f32)>> {
    let mut out = Vec::with_capacity(points.len());
    for &(x, y) in points {
        let (wx, wy) = match xform {
            None => (f64::from(x), f64::from(y)),
            Some(t) => t.transform_point(f64::from(x), f64::from(y)).ok()?,
        };
        out.push(world_to_pixel((wx, wy), plan.bbox, plan.width, plan.height));
    }
    Some(out)
}

fn cumulative_arc_length(pts: &[(f32, f32)]) -> Vec<f32> {
    let mut acc = 0.0_f32;
    let mut out = Vec::with_capacity(pts.len());
    out.push(0.0);
    for w in pts.windows(2) {
        let dx = w[1].0 - w[0].0;
        let dy = w[1].1 - w[0].1;
        acc += (dx * dx + dy * dy).sqrt();
        out.push(acc);
    }
    out
}

struct ArcSample {
    pos: (f32, f32),
    tangent: (f32, f32),
}

fn sample_at(pts: &[(f32, f32)], cum: &[f32], target: f32) -> Option<ArcSample> {
    if pts.len() < 2 || cum.len() != pts.len() {
        return None;
    }
    // first segment whose end >= target. linear scan; polylines on the hot
    // path are short enough to make a binary search overhead-not-worth-it.
    for i in 0..pts.len() - 1 {
        let s0 = cum[i];
        let s1 = cum[i + 1];
        let seg_len = s1 - s0;
        if target >= s0 && (target <= s1 || i + 1 == pts.len() - 1) {
            let t = if seg_len > 0.0 {
                ((target - s0) / seg_len).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let (x0, y0) = pts[i];
            let (x1, y1) = pts[i + 1];
            let dx = x1 - x0;
            let dy = y1 - y0;
            let len = (dx * dx + dy * dy).sqrt();
            if !(len.is_finite() && len > 0.0) {
                continue;
            }
            return Some(ArcSample {
                pos: (x0 + dx * t, y0 + dy * t),
                tangent: (dx / len, dy / len),
            });
        }
    }
    None
}

fn atan2_signed((tx, ty): (f32, f32)) -> f32 {
    ty.atan2(tx)
}

fn angle_diff(a: f32, b: f32) -> f32 {
    let mut d = a - b;
    while d > std::f32::consts::PI {
        d -= std::f32::consts::TAU;
    }
    while d < -std::f32::consts::PI {
        d += std::f32::consts::TAU;
    }
    d
}

/// AABB of a label's footprint after rotation. rotates the four corners of
/// the unrotated bbox around the anchor and takes their extent.
fn rotated_label_bbox(anchor: (f32, f32), half_w: f32, half_h: f32, angle_rad: f32) -> (f32, f32, f32, f32) {
    let (sin_a, cos_a) = angle_rad.sin_cos();
    let corners = [
        (-half_w, -half_h),
        (half_w, -half_h),
        (half_w, half_h),
        (-half_w, half_h),
    ];
    let mut minx = f32::INFINITY;
    let mut miny = f32::INFINITY;
    let mut maxx = f32::NEG_INFINITY;
    let mut maxy = f32::NEG_INFINITY;
    for (lx, ly) in corners {
        let rx = anchor.0 + cos_a * lx - sin_a * ly;
        let ry = anchor.1 + sin_a * lx + cos_a * ly;
        if rx < minx {
            minx = rx;
        }
        if ry < miny {
            miny = ry;
        }
        if rx > maxx {
            maxx = rx;
        }
        if ry > maxy {
            maxy = ry;
        }
    }
    (minx, miny, maxx, maxy)
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn cumulative_arc_length_sums_segments() {
        let pts = vec![(0.0_f32, 0.0_f32), (3.0, 0.0), (3.0, 4.0)];
        let c = cumulative_arc_length(&pts);
        assert!((c[0] - 0.0).abs() < 1e-3);
        assert!((c[1] - 3.0).abs() < 1e-3);
        assert!((c[2] - 7.0).abs() < 1e-3);
    }

    #[test]
    fn sample_at_returns_position_and_unit_tangent() {
        let pts = vec![(0.0_f32, 0.0_f32), (10.0, 0.0), (10.0, 10.0)];
        let c = cumulative_arc_length(&pts);
        let s = sample_at(&pts, &c, 5.0).unwrap();
        assert!((s.pos.0 - 5.0).abs() < 1e-3);
        assert!(s.pos.1.abs() < 1e-3);
        assert!((s.tangent.0 - 1.0).abs() < 1e-3);
        // sample mid second segment: tangent points straight down (y-down).
        let s2 = sample_at(&pts, &c, 15.0).unwrap();
        assert!((s2.pos.0 - 10.0).abs() < 1e-3);
        assert!((s2.pos.1 - 5.0).abs() < 1e-3);
        assert!(s2.tangent.0.abs() < 1e-3);
        assert!((s2.tangent.1 - 1.0).abs() < 1e-3);
    }

    #[test]
    fn rotated_bbox_swaps_extent_at_quarter_turn() {
        // 90deg rotation swaps the bbox axes; width<->height. 40x10 -> 10x40.
        let axis = rotated_label_bbox((100.0, 100.0), 20.0, 5.0, 0.0);
        let rot = rotated_label_bbox((100.0, 100.0), 20.0, 5.0, std::f32::consts::FRAC_PI_2);
        let axis_w = axis.2 - axis.0;
        let axis_h = axis.3 - axis.1;
        let rot_w = rot.2 - rot.0;
        let rot_h = rot.3 - rot.1;
        assert!((axis_w - 40.0).abs() < 1e-3, "axis width: {axis_w}");
        assert!((rot_w - 10.0).abs() < 1e-3, "rot width: {rot_w}");
        assert!((rot_h - 40.0).abs() < 1e-3, "rot height: {rot_h}");
        assert!((axis_h - 10.0).abs() < 1e-3, "axis height: {axis_h}");
    }

    #[test]
    fn angle_diff_wraps_through_pi() {
        let d = angle_diff(3.0, -3.0);
        // 3 and -3 differ by 6 in the naive sense, but wrap to ~0.28 around
        // the circle.
        assert!(d.abs() < 0.5, "got {d}");
    }
}

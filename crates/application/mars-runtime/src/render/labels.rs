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
use mars_style::{AnchorPosition, LabelStyle, Placement, Stylesheet};
use mars_types::BindingMetadata;

use crate::{RenderPlan, RuntimeError};

use super::decode::ClassResolver;
use super::project::world_to_pixel;
use super::{map_artifact_err, map_proj_err};

/// label candidate that has been resolved against the active stylesheet and
/// projected into request-CRS pixel space. carries enough state for the
/// collision pass to keep or drop it without redoing the projection.
pub(super) struct PreparedLabel {
    /// raw geometry-anchor in pixel space, pre-POSITION, pre-OFFSET. the
    /// collision pass adds the chosen placement's label-local offset (after
    /// rotating by `angle_rad`) to obtain the final baseline anchor.
    raw_anchor_px: (f32, f32),
    text: String,
    style: Arc<LabelStyle>,
    priority: u16,
    /// counter-clockwise rotation in radians; non-zero for rotated labels
    /// (line tangent or explicit ANGLE).
    angle_rad: f32,
    /// resolved POSITION (with OFFSET folded in) - one fixed candidate or
    /// an AUTO set the collision pass tries in order.
    placement: PreparedPlacement,
}

/// resolved POSITION decision for a single `PreparedLabel`. `Fixed` carries
/// the only bbox the collision pass should consider; `Auto` carries the
/// ordered set of candidate placements to try.
pub(super) enum PreparedPlacement {
    Fixed {
        anchor_offset_px: (f32, f32),
        bbox_px: (f32, f32, f32, f32),
    },
    Auto {
        candidates: Vec<PositionCandidate>,
    },
}

/// one POSITION candidate inside [`PreparedPlacement::Auto`]. carries the
/// label-local-frame offset from `raw_anchor_px` to the would-be baseline
/// anchor, plus the bbox the collision pass tests against.
pub(super) struct PositionCandidate {
    anchor_offset_px: (f32, f32),
    bbox_px: (f32, f32, f32, f32),
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
                ..
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
        let raw_anchor_px = world_to_pixel(anchor_world, plan.bbox, plan.width, plan.height);
        if !inside_pixel_canvas(raw_anchor_px, plan.width, plan.height) {
            continue;
        }
        let metrics = match renderer.measure_text(&c.text, &style) {
            Ok(m) => m,
            // font lookup / shaping failure: drop the candidate. matches the
            // existing "drop on error" behaviour of style/anchor resolution
            // a few lines above.
            Err(_) => continue,
        };
        // angle resolution: explicit numeric ANGLE wins over the
        // placement-derived angle (which is zero for the point/polygon path).
        let angle_rad = effective_angle_rad(&style, 0.0);
        let placement = build_placement(raw_anchor_px, &metrics, &style, angle_rad);
        out.push(PreparedLabel {
            raw_anchor_px,
            text: c.text,
            style,
            priority: c.priority,
            angle_rad,
            placement,
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
        // numeric ANGLE wins over the tangent if set on the style. mirrors
        // mapserver: explicit `ANGLE <deg>` overrides AUTO/FOLLOW.
        let angle = effective_angle_rad(style, angle);
        if !inside_pixel_canvas(centre.pos, plan.width, plan.height) {
            continue;
        }
        let placement = build_placement(centre.pos, &metrics, style, angle);
        out.push(PreparedLabel {
            raw_anchor_px: centre.pos,
            text: text.to_owned(),
            style: style.clone(),
            priority,
            angle_rad: angle,
            placement,
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

/// font-aware bbox around `anchor` at `angle_rad`. axis-aligned for the
/// `angle ≈ 0` fast path; rotated bbox otherwise.
fn label_bbox(anchor: (f32, f32), m: &mars_render_port::TextMetrics, angle_rad: f32) -> (f32, f32, f32, f32) {
    if angle_rad.abs() < f32::EPSILON {
        return text_bbox_from_metrics(anchor, *m);
    }
    let half_w = m.advance_x * 0.5;
    let half_h = m.ascent.max(m.descent);
    rotated_label_bbox(anchor, half_w, half_h, angle_rad)
}

/// effective label angle: explicit numeric `ANGLE <deg>` on the style
/// overrides whatever the placement step derived (zero for points/polys,
/// tangent for AUTO line labels).
fn effective_angle_rad(style: &LabelStyle, placement_angle: f32) -> f32 {
    match style.angle_deg {
        Some(deg) => deg.to_radians(),
        None => placement_angle,
    }
}

/// label-local-frame offset from the geometry point to the baseline anchor
/// for a given POSITION keyword. mapserver semantics: POSITION names where
/// the label sits *relative to the point*, so e.g. `Uc` puts the label
/// above the point with its bottom edge through the point.
///
/// pixel y grows downward; positive y in label-local frame is "below the
/// baseline", which matches descent.
fn anchor_offset_for_position(pos: AnchorPosition, m: &mars_render_port::TextMetrics) -> (f32, f32) {
    let half_w = m.advance_x * 0.5;
    // vertical centre of the bbox relative to baseline, in label-local frame.
    // bbox spans [-ascent, +descent]; midpoint is (descent - ascent) / 2.
    let centre_y = (m.descent - m.ascent) * 0.5;
    match pos {
        AnchorPosition::Ul => (-half_w, -m.descent),
        AnchorPosition::Uc => (0.0, -m.descent),
        AnchorPosition::Ur => (half_w, -m.descent),
        AnchorPosition::Cl => (-half_w, -centre_y),
        AnchorPosition::Cc | AnchorPosition::Auto => (0.0, -centre_y),
        AnchorPosition::Cr => (half_w, -centre_y),
        AnchorPosition::Ll => (-half_w, m.ascent),
        AnchorPosition::Lc => (0.0, m.ascent),
        AnchorPosition::Lr => (half_w, m.ascent),
    }
}

/// POSITION AUTO candidate order. mapserver's AUTO walks the perimeter
/// trying for a non-colliding placement; CC is skipped because it sits on
/// the geometry point itself and offers no escape from overlap.
const AUTO_POSITIONS: [AnchorPosition; 8] = [
    AnchorPosition::Uc,
    AnchorPosition::Lc,
    AnchorPosition::Cr,
    AnchorPosition::Cl,
    AnchorPosition::Ur,
    AnchorPosition::Ll,
    AnchorPosition::Ul,
    AnchorPosition::Lr,
];

/// build the [`PreparedPlacement`] for a candidate from its style POSITION
/// keyword + OFFSET. all bboxes are in canvas frame and ready for the
/// collision pass; the chosen offset gets re-applied via [`apply_offset`]
/// at emit time so the final anchor and bbox stay consistent.
fn build_placement(
    raw_anchor_px: (f32, f32),
    metrics: &mars_render_port::TextMetrics,
    style: &LabelStyle,
    angle_rad: f32,
) -> PreparedPlacement {
    let make = |pos: AnchorPosition| -> PositionCandidate {
        // POSITION offset is in label-local frame; OFFSET adds on top, also
        // in label-local frame (rotates with ANGLE per mapserver semantics).
        let pos_off = anchor_offset_for_position(pos, metrics);
        let local_offset = (pos_off.0 + style.offset_px.0, pos_off.1 + style.offset_px.1);
        let final_anchor = apply_offset(raw_anchor_px, local_offset, angle_rad);
        let bbox_px = label_bbox(final_anchor, metrics, angle_rad);
        PositionCandidate {
            anchor_offset_px: local_offset,
            bbox_px,
        }
    };
    match style.position {
        AnchorPosition::Auto => {
            let candidates = AUTO_POSITIONS.iter().copied().map(make).collect();
            PreparedPlacement::Auto { candidates }
        }
        fixed => {
            let c = make(fixed);
            PreparedPlacement::Fixed {
                anchor_offset_px: c.anchor_offset_px,
                bbox_px: c.bbox_px,
            }
        }
    }
}

/// shift an anchor by the style's OFFSET, in canvas frame when the label
/// is axis-aligned, in label-local frame (rotates with the run) otherwise.
/// matches mapserver semantics for `OFFSET <x> <y>` under non-zero ANGLE.
fn apply_offset(anchor: (f32, f32), offset_px: (f32, f32), angle_rad: f32) -> (f32, f32) {
    let (dx, dy) = offset_px;
    if dx == 0.0 && dy == 0.0 {
        return anchor;
    }
    if angle_rad.abs() < f32::EPSILON {
        return (anchor.0 + dx, anchor.1 + dy);
    }
    let (sin_a, cos_a) = angle_rad.sin_cos();
    let rx = cos_a * dx - sin_a * dy;
    let ry = sin_a * dx + cos_a * dy;
    (anchor.0 + rx, anchor.1 + ry)
}

/// run a greedy collision pass over the accumulated label set and return
/// the surviving `DrawOp::Label` ops in placement order. each placed label
/// remembers its `min_distance`; collision against a candidate uses the
/// max of the two values, so the wider neighbour wins per pair (mirrors
/// mapserver's `MINDISTANCE`, post-7.2 pixel semantics). AUTO-positioned
/// labels try each candidate placement in mapserver order; the first
/// non-colliding one is placed.
pub(super) fn collide_and_emit_labels(mut labels: Vec<PreparedLabel>, _w: u32, _h: u32) -> Vec<DrawOp> {
    if labels.is_empty() {
        return Vec::new();
    }
    // priority desc → place high-priority labels first, drop conflicts.
    labels.sort_by_key(|l| std::cmp::Reverse(l.priority));
    let mut placed: Vec<PlacedFootprint> = Vec::with_capacity(labels.len());
    let mut ops = Vec::with_capacity(labels.len());
    for label in labels {
        let cand_md = label.style.min_distance.max(0.0);
        let chosen = pick_placement(&label.placement, cand_md, &placed);
        let Some((anchor_offset, bbox_used)) = chosen else {
            continue;
        };
        let anchor = apply_offset(label.raw_anchor_px, anchor_offset, label.angle_rad);
        placed.push(PlacedFootprint {
            bbox: bbox_used,
            min_distance: cand_md,
        });
        ops.push(DrawOp::Label {
            anchor,
            text: label.text,
            style: label.style,
            angle_rad: label.angle_rad,
        });
    }
    ops
}

/// chosen placement output: `(label-local-frame anchor offset, bbox in
/// canvas frame)`.
type ChosenPlacement = ((f32, f32), (f32, f32, f32, f32));

/// pick the first non-colliding candidate from a [`PreparedPlacement`].
/// returns `(label-local-frame anchor offset, bbox in canvas frame)` for
/// the chosen placement, or `None` when no candidate fits.
fn pick_placement(
    placement: &PreparedPlacement,
    cand_md: f32,
    placed: &[PlacedFootprint],
) -> Option<ChosenPlacement> {
    match placement {
        PreparedPlacement::Fixed {
            anchor_offset_px,
            bbox_px,
        } => {
            if collides(*bbox_px, cand_md, placed) {
                None
            } else {
                Some((*anchor_offset_px, *bbox_px))
            }
        }
        PreparedPlacement::Auto { candidates } => candidates
            .iter()
            .find(|c| !collides(c.bbox_px, cand_md, placed))
            .map(|c| (c.anchor_offset_px, c.bbox_px)),
    }
}

fn collides(bbox: (f32, f32, f32, f32), cand_md: f32, placed: &[PlacedFootprint]) -> bool {
    placed
        .iter()
        .any(|p| bboxes_within(bbox, p.bbox, cand_md.max(p.min_distance)))
}

struct PlacedFootprint {
    bbox: (f32, f32, f32, f32),
    min_distance: f32,
}

/// `true` when `a` inflated by `pad` on every side overlaps `b`. equivalent
/// to "the gap between the bboxes is < pad", so passing `pad == 0` reduces
/// to a plain overlap test.
fn bboxes_within(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32), pad: f32) -> bool {
    let pad = pad.max(0.0);
    let inflated = (a.0 - pad, a.1 - pad, a.2 + pad, a.3 + pad);
    pixel_bbox_overlaps(inflated, b)
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

    fn prepared(bbox: (f32, f32, f32, f32), priority: u16, min_distance: f32) -> PreparedLabel {
        PreparedLabel {
            raw_anchor_px: (0.0, 0.0),
            text: String::new(),
            style: Arc::new(LabelStyle {
                font_family: String::new(),
                font_size: 12.0,
                fill: mars_style::Colour::rgba(0, 0, 0, 255),
                halo: None,
                priority,
                min_distance,
                position: AnchorPosition::default(),
                offset_px: (0.0, 0.0),
                angle_deg: None,
                partials: false,
                force: false,
            }),
            priority,
            angle_rad: 0.0,
            placement: PreparedPlacement::Fixed {
                anchor_offset_px: (0.0, 0.0),
                bbox_px: bbox,
            },
        }
    }

    fn metrics_8x4() -> mars_render_port::TextMetrics {
        // half_w = 4; ascent + descent = 4 vertical; matches a hand-friendly
        // 8x4-pixel bbox centred at the baseline anchor.
        mars_render_port::TextMetrics {
            advance_x: 8.0,
            ascent: 3.0,
            descent: 1.0,
        }
    }

    #[test]
    fn collision_drops_overlapping_bboxes() {
        // both at the same bbox; second one (lower priority) drops.
        let a = prepared((0.0, 0.0, 10.0, 10.0), 10, 0.0);
        let b = prepared((0.0, 0.0, 10.0, 10.0), 5, 0.0);
        let ops = collide_and_emit_labels(vec![a, b], 100, 100);
        assert_eq!(ops.len(), 1, "second overlapping label must drop");
    }

    #[test]
    fn mindistance_pad_drops_non_overlapping_but_close_bboxes() {
        // 5 px gap between two 10x10 bboxes; with mindistance=10 the
        // candidate is rejected (gap < 10). priority order matters; the
        // first placed wins.
        let a = prepared((0.0, 0.0, 10.0, 10.0), 10, 10.0);
        let b = prepared((15.0, 0.0, 25.0, 10.0), 5, 10.0);
        let ops = collide_and_emit_labels(vec![a, b], 100, 100);
        assert_eq!(ops.len(), 1, "second label within mindistance must drop");
    }

    #[test]
    fn mindistance_pad_allows_bboxes_outside_the_inflation() {
        // 20 px gap > mindistance 10; both survive.
        let a = prepared((0.0, 0.0, 10.0, 10.0), 10, 10.0);
        let b = prepared((30.0, 0.0, 40.0, 10.0), 5, 10.0);
        let ops = collide_and_emit_labels(vec![a, b], 100, 100);
        assert_eq!(ops.len(), 2, "labels beyond mindistance must both place");
    }

    #[test]
    fn mindistance_uses_max_of_the_two_values_per_pair() {
        // placed label has mindistance 0; candidate has mindistance 12.
        // gap is 10 < max(0, 12) = 12, so the candidate is rejected.
        let a = prepared((0.0, 0.0, 10.0, 10.0), 10, 0.0);
        let b = prepared((20.0, 0.0, 30.0, 10.0), 5, 12.0);
        let ops = collide_and_emit_labels(vec![a, b], 100, 100);
        assert_eq!(ops.len(), 1, "candidate's wider mindistance must apply");
    }

    #[test]
    fn anchor_offset_resolves_each_position_keyword() {
        let m = metrics_8x4();
        // half_w = 4, ascent = 3, descent = 1; centre_y = -1
        assert_eq!(anchor_offset_for_position(AnchorPosition::Ul, &m), (-4.0, -1.0));
        assert_eq!(anchor_offset_for_position(AnchorPosition::Uc, &m), (0.0, -1.0));
        assert_eq!(anchor_offset_for_position(AnchorPosition::Ur, &m), (4.0, -1.0));
        assert_eq!(anchor_offset_for_position(AnchorPosition::Cl, &m), (-4.0, 1.0));
        assert_eq!(anchor_offset_for_position(AnchorPosition::Cc, &m), (0.0, 1.0));
        assert_eq!(anchor_offset_for_position(AnchorPosition::Cr, &m), (4.0, 1.0));
        assert_eq!(anchor_offset_for_position(AnchorPosition::Ll, &m), (-4.0, 3.0));
        assert_eq!(anchor_offset_for_position(AnchorPosition::Lc, &m), (0.0, 3.0));
        assert_eq!(anchor_offset_for_position(AnchorPosition::Lr, &m), (4.0, 3.0));
        // Auto falls back to CC for the per-position helper; the candidate
        // walk lives in `build_placement`.
        assert_eq!(anchor_offset_for_position(AnchorPosition::Auto, &m), (0.0, 1.0));
    }

    #[test]
    fn auto_position_walk_skips_cc_and_covers_eight_perimeter_positions() {
        assert_eq!(AUTO_POSITIONS.len(), 8);
        assert!(!AUTO_POSITIONS.contains(&AnchorPosition::Cc));
        assert!(!AUTO_POSITIONS.contains(&AnchorPosition::Auto));
        // each entry appears exactly once.
        for p in AUTO_POSITIONS {
            let count = AUTO_POSITIONS.iter().filter(|q| **q == p).count();
            assert_eq!(count, 1, "duplicate AUTO candidate: {p:?}");
        }
    }

    #[test]
    fn build_placement_auto_picks_first_non_colliding_candidate() {
        // 1 candidate placed at the geometry point's UC position (label
        // sits above the point). build a second label with AUTO and a
        // bbox that collides at UC but not at LC. expect the second to
        // land in the LC slot.
        let m = metrics_8x4();
        let style_uc = Arc::new(LabelStyle {
            font_family: String::new(),
            font_size: 12.0,
            fill: mars_style::Colour::rgba(0, 0, 0, 255),
            halo: None,
            priority: 0,
            min_distance: 0.0,
            position: AnchorPosition::Uc,
            offset_px: (0.0, 0.0),
            angle_deg: None,
            partials: false,
            force: false,
        });
        let mut style_auto = (*style_uc).clone();
        style_auto.position = AnchorPosition::Auto;
        let style_auto = Arc::new(style_auto);
        let first = PreparedLabel {
            raw_anchor_px: (50.0, 50.0),
            text: String::new(),
            style: style_uc.clone(),
            priority: 10,
            angle_rad: 0.0,
            placement: build_placement((50.0, 50.0), &m, &style_uc, 0.0),
        };
        let second = PreparedLabel {
            raw_anchor_px: (50.0, 50.0),
            text: String::new(),
            style: style_auto.clone(),
            priority: 5,
            angle_rad: 0.0,
            placement: build_placement((50.0, 50.0), &m, &style_auto, 0.0),
        };
        let ops = collide_and_emit_labels(vec![first, second], 200, 200);
        assert_eq!(ops.len(), 2, "AUTO must find an alternate slot");
        // ensure the AUTO label landed below the point (Lc), not at the
        // same UC slot as the placed one. Lc anchor_y is raw + ascent (3.0)
        // → 50 + 3 = 53; Uc would have been 50 - descent (1.0) = 49.
        if let DrawOp::Label { anchor, .. } = &ops[1] {
            assert!(anchor.1 > 50.0, "AUTO should escape downward; got {anchor:?}");
        } else {
            panic!("expected Label op");
        }
    }

    #[test]
    fn build_placement_auto_drops_when_all_candidates_collide() {
        // place a giant occupier covering the whole search area, then drop
        // an AUTO candidate at the same point. no slot fits.
        let m = metrics_8x4();
        let style_force = Arc::new(LabelStyle {
            font_family: String::new(),
            font_size: 12.0,
            fill: mars_style::Colour::rgba(0, 0, 0, 255),
            halo: None,
            priority: 10,
            min_distance: 0.0,
            position: AnchorPosition::Cc,
            offset_px: (0.0, 0.0),
            angle_deg: None,
            partials: false,
            force: false,
        });
        let mut style_auto = (*style_force).clone();
        style_auto.position = AnchorPosition::Auto;
        let style_auto = Arc::new(style_auto);
        let occupier = PreparedLabel {
            raw_anchor_px: (100.0, 100.0),
            text: String::new(),
            style: style_force.clone(),
            priority: 100,
            angle_rad: 0.0,
            placement: PreparedPlacement::Fixed {
                anchor_offset_px: (0.0, 0.0),
                bbox_px: (50.0, 50.0, 150.0, 150.0),
            },
        };
        let candidate = PreparedLabel {
            raw_anchor_px: (100.0, 100.0),
            text: String::new(),
            style: style_auto.clone(),
            priority: 5,
            angle_rad: 0.0,
            placement: build_placement((100.0, 100.0), &m, &style_auto, 0.0),
        };
        let ops = collide_and_emit_labels(vec![occupier, candidate], 200, 200);
        assert_eq!(ops.len(), 1, "all AUTO candidates inside the occupier must drop");
    }

    #[test]
    fn effective_angle_picks_style_override_when_set() {
        let mut s = LabelStyle {
            font_family: String::new(),
            font_size: 12.0,
            fill: mars_style::Colour::rgba(0, 0, 0, 255),
            halo: None,
            priority: 0,
            min_distance: 0.0,
            position: mars_style::AnchorPosition::default(),
            offset_px: (0.0, 0.0),
            angle_deg: None,
            partials: false,
            force: false,
        };
        // no override: placement angle passes through
        assert!((effective_angle_rad(&s, 1.0) - 1.0).abs() < 1e-6);
        // override: degrees → radians, placement angle ignored
        s.angle_deg = Some(90.0);
        assert!((effective_angle_rad(&s, 1.0) - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
    }

    #[test]
    fn apply_offset_in_canvas_frame_when_axis_aligned() {
        let a = apply_offset((100.0, 200.0), (5.0, -3.0), 0.0);
        assert!((a.0 - 105.0).abs() < 1e-6);
        assert!((a.1 - 197.0).abs() < 1e-6);
    }

    #[test]
    fn apply_offset_rotates_with_label_frame_when_rotated() {
        // 90° rotation: offset (5, 0) in label frame -> (0, 5) in canvas frame.
        let a = apply_offset((100.0, 200.0), (5.0, 0.0), std::f32::consts::FRAC_PI_2);
        assert!((a.0 - 100.0).abs() < 1e-4, "got {}", a.0);
        assert!((a.1 - 205.0).abs() < 1e-4, "got {}", a.1);
    }

    #[test]
    fn apply_offset_is_noop_for_zero_offset() {
        let a = apply_offset((10.0, 20.0), (0.0, 0.0), 1.5);
        assert_eq!(a, (10.0, 20.0));
    }

    #[test]
    fn negative_mindistance_treated_as_zero() {
        // gap is 1 px; with mindistance < 0 we behave as plain overlap test
        // (both should place: no overlap, no padding).
        let a = prepared((0.0, 0.0, 10.0, 10.0), 10, -5.0);
        let b = prepared((11.0, 0.0, 21.0, 10.0), 5, -5.0);
        let ops = collide_and_emit_labels(vec![a, b], 100, 100);
        assert_eq!(ops.len(), 2, "negative mindistance clamps to 0");
    }
}

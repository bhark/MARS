//! `Placement::Line` sampler: emits one prepared label per accepted point along
//! the polyline.

use std::sync::Arc;

use mars_render_port::Renderer;
use mars_style::{LineAngleMode, ResolvedLabelStyle};

use crate::RenderPlan;

use super::candidate::{PreparedLabel, PreparedPlacement};
use super::geometry::{effective_angle_rad, filter_for_partials, inside_pixel_canvas, rotated_label_bbox};
use super::position::build_placement;
use super::projection::{angle_diff, atan2_signed, cumulative_arc_length, project_polyline_to_pixels, sample_at};

/// project the polyline to pixel space, walk it at `repeat_m` intervals
/// (converted to pixels via the plan's pixel size), and emit one
/// `PreparedLabel` per accepted sample. each sample carries the pixel-space
/// tangent angle and a rotated bbox for the collision pass.
#[allow(clippy::too_many_arguments)]
pub(super) fn sample_polyline_labels(
    points: &[(f32, f32)],
    xform: Option<&mars_proj::Transformer>,
    plan: &RenderPlan,
    repeat_m: f64,
    max_angle_delta_deg: f32,
    angle_mode: LineAngleMode,
    text: &str,
    priority: u16,
    style: &Arc<ResolvedLabelStyle>,
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
        // FOLLOW: per-glyph placement along the polyline. POSITION/OFFSET
        // do not compose with FOLLOW in v1 - the polyline path itself
        // anchors each glyph. numeric ANGLE also wins back to AUTO
        // semantics (one fixed orientation makes per-glyph rotation
        // meaningless) - drop to the block path when angle_deg is set.
        let use_follow = matches!(angle_mode, LineAngleMode::Follow) && style.angle_deg.is_none();
        if use_follow {
            // bbox for collision: the same rotated AABB the AUTO path uses,
            // sized by the run's centre tangent. tight enough at moderate
            // curvatures (the max_angle_delta_deg gate above already
            // rejects sharp bends across the label footprint).
            let half_h = metrics.ascent.max(metrics.descent);
            let bbox_px = rotated_label_bbox(centre.pos, half_advance, half_h, angle);
            let placement = PreparedPlacement::Follow {
                polyline_px: pixel_pts.clone(),
                start_arc_px: pos - half_advance,
                bbox_px,
            };
            let Some(placement) = filter_for_partials(placement, style.partials, plan.width, plan.height) else {
                continue;
            };
            out.push(PreparedLabel {
                raw_anchor_px: centre.pos,
                text: text.to_owned(),
                style: style.clone(),
                priority,
                angle_rad: angle,
                placement,
            });
            continue;
        }
        let placement = build_placement(centre.pos, &metrics, style, angle);
        let Some(placement) = filter_for_partials(placement, style.partials, plan.width, plan.height) else {
            continue;
        };
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

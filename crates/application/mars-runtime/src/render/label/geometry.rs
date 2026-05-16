//! label bbox + anchor math shared between placement and collision.

use mars_artifact::{LabelCandidate, LabelShape};
use mars_render_port::TextMetrics;
use mars_style::ResolvedLabelStyle;

/// AABB of a label's footprint after rotation. rotates the four corners of
/// the unrotated bbox around the anchor and takes their extent.
pub(super) fn rotated_label_bbox(anchor: (f32, f32), half_w: f32, half_h: f32, angle_rad: f32) -> (f32, f32, f32, f32) {
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

pub(super) fn label_anchor_world(c: &LabelCandidate, xform: Option<&mars_proj::Transformer>) -> Option<(f64, f64)> {
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

pub(super) fn inside_pixel_canvas(p: (f32, f32), w: u32, h: u32) -> bool {
    p.0 >= 0.0 && p.1 >= 0.0 && p.0 <= w as f32 && p.1 <= h as f32
}

/// build a pixel-space bbox around `anchor` from the font-aware metrics the
/// renderer would use to rasterise the same run. anchor is the baseline
/// origin; bbox extends by half advance horizontally and by ascent / descent
/// vertically. centred horizontally because draw_label paints around the
/// anchor; the vertical extent uses the actual font ascent + descent so the
/// collision bbox matches what tiny-skia paints.
pub(super) fn text_bbox_from_metrics(anchor: (f32, f32), m: TextMetrics) -> (f32, f32, f32, f32) {
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
pub(super) fn label_bbox(anchor: (f32, f32), m: &TextMetrics, angle_rad: f32) -> (f32, f32, f32, f32) {
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
pub(super) fn effective_angle_rad(style: &ResolvedLabelStyle, placement_angle: f32) -> f32 {
    match style.angle_deg {
        Some(deg) => deg.to_radians(),
        None => placement_angle,
    }
}

/// shift an anchor by the style's OFFSET, in canvas frame when the label
/// is axis-aligned, in label-local frame (rotates with the run) otherwise.
/// matches mapserver semantics for `OFFSET <x> <y>` under non-zero ANGLE.
pub(super) fn apply_offset(anchor: (f32, f32), offset_px: (f32, f32), angle_rad: f32) -> (f32, f32) {
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

/// `true` when `bbox` lies fully inside the `(0, 0)..(w, h)` canvas.
fn bbox_inside_canvas(bbox: (f32, f32, f32, f32), w: u32, h: u32) -> bool {
    let (w, h) = (w as f32, h as f32);
    bbox.0 >= 0.0 && bbox.1 >= 0.0 && bbox.2 <= w && bbox.3 <= h
}

/// drop candidates whose bbox doesn't fit inside the canvas. mirrors
/// mapserver's `PARTIALS FALSE`. for AUTO, filter the candidate list and
/// drop the whole label if nothing survives.
pub(super) fn filter_for_partials(
    placement: super::candidate::PreparedPlacement,
    partials: bool,
    w: u32,
    h: u32,
) -> Option<super::candidate::PreparedPlacement> {
    use super::candidate::{PositionCandidate, PreparedPlacement};
    if partials {
        return Some(placement);
    }
    match placement {
        PreparedPlacement::Fixed {
            anchor_offset_px,
            bbox_px,
        } => {
            if bbox_inside_canvas(bbox_px, w, h) {
                Some(PreparedPlacement::Fixed {
                    anchor_offset_px,
                    bbox_px,
                })
            } else {
                None
            }
        }
        PreparedPlacement::Auto { candidates } => {
            let kept: Vec<PositionCandidate> = candidates
                .into_iter()
                .filter(|c| bbox_inside_canvas(c.bbox_px, w, h))
                .collect();
            if kept.is_empty() {
                None
            } else {
                Some(PreparedPlacement::Auto { candidates: kept })
            }
        }
        PreparedPlacement::Follow {
            polyline_px,
            start_arc_px,
            bbox_px,
        } => {
            if bbox_inside_canvas(bbox_px, w, h) {
                Some(PreparedPlacement::Follow {
                    polyline_px,
                    start_arc_px,
                    bbox_px,
                })
            } else {
                None
            }
        }
    }
}

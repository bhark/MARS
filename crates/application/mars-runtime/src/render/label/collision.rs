//! greedy collision pass + bbox overlap math.

use mars_render_port::DrawOp;

use super::candidate::{PreparedLabel, PreparedPlacement};
use super::geometry::apply_offset;

/// run a greedy collision pass over the accumulated label set and return
/// the surviving `DrawOp::Label` ops in placement order. each placed label
/// remembers its `min_distance`; collision against a candidate uses the
/// max of the two values, so the wider neighbour wins per pair (mirrors
/// mapserver's `MINDISTANCE`, post-7.2 pixel semantics). AUTO-positioned
/// labels try each candidate placement in mapserver order; the first
/// non-colliding one is placed.
#[doc(hidden)]
#[allow(unreachable_pub)]
pub fn collide_and_emit_labels(mut labels: Vec<PreparedLabel>, _w: u32, _h: u32) -> Vec<DrawOp> {
    if labels.is_empty() {
        return Vec::new();
    }
    // force-first, then priority desc. forced labels are placed regardless
    // of collision; sorting them up front means subsequent labels see them
    // as obstacles and can dodge.
    labels.sort_by_key(|l| (std::cmp::Reverse(l.style.force), std::cmp::Reverse(l.priority)));
    let mut placed: Vec<PlacedFootprint> = Vec::with_capacity(labels.len());
    let mut ops = Vec::with_capacity(labels.len());
    for label in labels {
        let cand_md = label.style.min_distance.max(0.0);
        let force = label.style.force;
        // Follow labels emit a different DrawOp variant, so split the
        // collision + emit path off here. block placements (Fixed/Auto)
        // share the existing pick_placement helper.
        match label.placement {
            PreparedPlacement::Follow {
                polyline_px,
                start_arc_px,
                bbox_px,
            } => {
                if !force && collides(bbox_px, cand_md, &placed) {
                    continue;
                }
                placed.push(PlacedFootprint {
                    bbox: bbox_px,
                    min_distance: cand_md,
                });
                ops.push(DrawOp::FollowLabel {
                    polyline_px,
                    start_arc_px,
                    text: label.text,
                    style: label.style,
                });
            }
            placement @ (PreparedPlacement::Fixed { .. } | PreparedPlacement::Auto { .. }) => {
                let chosen = if force {
                    force_pick(&placement)
                } else {
                    pick_placement(&placement, cand_md, &placed)
                };
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
        }
    }
    ops
}

/// FORCE bypass for block (non-FOLLOW) placements: take the only `Fixed`
/// slot or the first `Auto` candidate. no collision test, no AUTO dodging.
/// mirrors mapserver `FORCE`.
fn force_pick(placement: &PreparedPlacement) -> Option<ChosenPlacement> {
    match placement {
        PreparedPlacement::Fixed {
            anchor_offset_px,
            bbox_px,
        } => Some((*anchor_offset_px, *bbox_px)),
        PreparedPlacement::Auto { candidates } => candidates.first().map(|c| (c.anchor_offset_px, c.bbox_px)),
        // Follow handled inline in the loop above; should never reach here.
        PreparedPlacement::Follow { .. } => None,
    }
}

/// chosen placement output: `(label-local-frame anchor offset, bbox in
/// canvas frame)`.
type ChosenPlacement = ((f32, f32), (f32, f32, f32, f32));

/// pick the first non-colliding candidate from a [`PreparedPlacement`].
/// returns `(label-local-frame anchor offset, bbox in canvas frame)` for
/// the chosen placement, or `None` when no candidate fits.
fn pick_placement(placement: &PreparedPlacement, cand_md: f32, placed: &[PlacedFootprint]) -> Option<ChosenPlacement> {
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
        // Follow is handled inline in `collide_and_emit_labels`; should
        // never reach pick_placement.
        PreparedPlacement::Follow { .. } => None,
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

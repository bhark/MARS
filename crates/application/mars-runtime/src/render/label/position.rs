//! POSITION + OFFSET resolution and the AUTO walk order.

use mars_render_port::TextMetrics;
use mars_style::{AnchorPosition, ResolvedLabelStyle};

use super::candidate::{PositionCandidate, PreparedPlacement};
use super::geometry::{apply_offset, label_bbox};

/// label-local-frame offset from the geometry point to the baseline anchor
/// for a given POSITION keyword. mapserver semantics: POSITION names where
/// the label sits *relative to the point*, so e.g. `Uc` puts the label
/// above the point with its bottom edge through the point.
///
/// pixel y grows downward; positive y in label-local frame is "below the
/// baseline", which matches descent.
pub(super) fn anchor_offset_for_position(pos: AnchorPosition, m: &TextMetrics) -> (f32, f32) {
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
pub(super) const AUTO_POSITIONS: [AnchorPosition; 8] = [
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
pub(super) fn build_placement(
    raw_anchor_px: (f32, f32),
    metrics: &TextMetrics,
    style: &ResolvedLabelStyle,
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

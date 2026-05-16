//! prepared label candidate types + bench-internals constructors.

use std::sync::Arc;

use mars_style::ResolvedLabelStyle;

/// label candidate that has been resolved against the active stylesheet and
/// projected into request-CRS pixel space. carries enough state for the
/// collision pass to keep or drop it without redoing the projection.
///
/// `pub` so the `bench-internals` feature can re-export it; the `render`
/// module is non-`pub` so this is not reachable without the feature gate.
#[doc(hidden)]
#[allow(unreachable_pub)]
#[derive(Clone)]
pub struct PreparedLabel {
    /// raw geometry-anchor in pixel space, pre-POSITION, pre-OFFSET. the
    /// collision pass adds the chosen placement's label-local offset (after
    /// rotating by `angle_rad`) to obtain the final baseline anchor.
    pub(super) raw_anchor_px: (f32, f32),
    pub(super) text: String,
    pub(super) style: Arc<ResolvedLabelStyle>,
    pub(super) priority: u16,
    /// counter-clockwise rotation in radians; non-zero for rotated labels
    /// (line tangent or explicit ANGLE).
    pub(super) angle_rad: f32,
    /// resolved POSITION (with OFFSET folded in) - one fixed candidate or
    /// an AUTO set the collision pass tries in order.
    pub(super) placement: PreparedPlacement,
}

/// resolved POSITION decision for a single `PreparedLabel`. `Fixed` carries
/// the only bbox the collision pass should consider; `Auto` carries the
/// ordered set of candidate placements to try; `Follow` carries the
/// polyline + starting arc-length so the renderer can lay out the glyphs
/// per-character along the curve.
#[doc(hidden)]
#[allow(unreachable_pub)]
#[derive(Clone)]
pub enum PreparedPlacement {
    Fixed {
        anchor_offset_px: (f32, f32),
        bbox_px: (f32, f32, f32, f32),
    },
    Auto {
        candidates: Vec<PositionCandidate>,
    },
    Follow {
        polyline_px: Vec<(f32, f32)>,
        start_arc_px: f32,
        bbox_px: (f32, f32, f32, f32),
    },
}

/// one POSITION candidate inside [`PreparedPlacement::Auto`]. carries the
/// label-local-frame offset from `raw_anchor_px` to the would-be baseline
/// anchor, plus the bbox the collision pass tests against.
#[doc(hidden)]
#[allow(unreachable_pub)]
#[derive(Clone)]
pub struct PositionCandidate {
    pub(super) anchor_offset_px: (f32, f32),
    pub(super) bbox_px: (f32, f32, f32, f32),
}

/// bench-only constructor. private fields stay opaque to callers; the only
/// supported use is composing synthetic inputs for the label-collision bench.
#[cfg(feature = "bench-internals")]
#[doc(hidden)]
pub fn new_prepared_label(
    raw_anchor_px: (f32, f32),
    text: String,
    style: Arc<ResolvedLabelStyle>,
    priority: u16,
    angle_rad: f32,
    placement: PreparedPlacement,
) -> PreparedLabel {
    PreparedLabel {
        raw_anchor_px,
        text,
        style,
        priority,
        angle_rad,
        placement,
    }
}

/// bench-only constructor for `PositionCandidate`. see [`new_prepared_label`].
#[cfg(feature = "bench-internals")]
#[doc(hidden)]
pub fn new_position_candidate(anchor_offset_px: (f32, f32), bbox_px: (f32, f32, f32, f32)) -> PositionCandidate {
    PositionCandidate {
        anchor_offset_px,
        bbox_px,
    }
}

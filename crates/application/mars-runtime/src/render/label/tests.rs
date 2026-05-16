#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use mars_render_port::{DrawOp, TextMetrics};
use mars_style::{AnchorPosition, ResolvedLabelStyle};

use super::candidate::{PositionCandidate, PreparedLabel, PreparedPlacement};
use super::collision::collide_and_emit_labels;
use super::geometry::{apply_offset, effective_angle_rad, filter_for_partials, rotated_label_bbox};
use super::position::{AUTO_POSITIONS, anchor_offset_for_position, build_placement};
use super::projection::{angle_diff, cumulative_arc_length, sample_at};

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
        style: Arc::new(ResolvedLabelStyle {
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

fn metrics_8x4() -> TextMetrics {
    // half_w = 4; ascent + descent = 4 vertical; matches a hand-friendly
    // 8x4-pixel bbox centred at the baseline anchor.
    TextMetrics {
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
    let style_uc = Arc::new(ResolvedLabelStyle {
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
    let style_force = Arc::new(ResolvedLabelStyle {
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

fn label_with(force: bool, partials: bool, priority: u16, bbox: (f32, f32, f32, f32)) -> PreparedLabel {
    let mut lbl = prepared(bbox, priority, 0.0);
    let mut style = (*lbl.style).clone();
    style.force = force;
    style.partials = partials;
    lbl.style = Arc::new(style);
    lbl
}

#[test]
fn force_label_outranks_priority_when_competing_for_the_same_bbox() {
    // forced low-priority sorts ahead of the high-priority normal label
    // (force-first rule), places, and acts as an obstacle so the normal
    // label drops. matches mapserver `FORCE` semantics: forced labels
    // are unconditional; everyone else still tests against them.
    let high = label_with(false, true, 100, (0.0, 0.0, 10.0, 10.0));
    let forced_low = label_with(true, true, 1, (0.0, 0.0, 10.0, 10.0));
    let ops = collide_and_emit_labels(vec![high, forced_low], 100, 100);
    assert_eq!(ops.len(), 1, "forced label wins the slot");
}

#[test]
fn force_label_survives_when_added_to_already_occupied_slot() {
    // pre-place a high-priority label, then a forced one at the same
    // bbox arrives. force is sorted first so it actually places
    // *before* the high-priority one - but the placement itself
    // bypasses collision. result: both survive, with the forced one
    // first in placement order.
    let forced_low = label_with(true, true, 1, (0.0, 0.0, 10.0, 10.0));
    let mid = label_with(false, true, 50, (20.0, 0.0, 30.0, 10.0));
    let ops = collide_and_emit_labels(vec![forced_low, mid], 100, 100);
    assert_eq!(ops.len(), 2);
}

#[test]
fn force_label_blocks_subsequent_normal_label_at_same_bbox() {
    // forced low-priority sorts ahead of normal high-priority and
    // occupies the bbox; the normal label then collides and drops.
    let forced = label_with(true, true, 1, (0.0, 0.0, 10.0, 10.0));
    let normal = label_with(false, true, 100, (0.0, 0.0, 10.0, 10.0));
    let ops = collide_and_emit_labels(vec![forced, normal], 100, 100);
    assert_eq!(ops.len(), 1, "forced label is still a collision obstacle");
}

#[test]
fn partials_false_drops_bbox_crossing_canvas_edge() {
    // canvas-internal: kept.
    let inside = PreparedPlacement::Fixed {
        anchor_offset_px: (0.0, 0.0),
        bbox_px: (1.0, 1.0, 9.0, 9.0),
    };
    assert!(filter_for_partials(inside, false, 10, 10).is_some());
    // touches the right edge by 1 px: dropped.
    let crossing = PreparedPlacement::Fixed {
        anchor_offset_px: (0.0, 0.0),
        bbox_px: (5.0, 5.0, 11.0, 9.0),
    };
    assert!(filter_for_partials(crossing, false, 10, 10).is_none());
    // partials=true: kept even when crossing.
    let crossing2 = PreparedPlacement::Fixed {
        anchor_offset_px: (0.0, 0.0),
        bbox_px: (5.0, 5.0, 11.0, 9.0),
    };
    assert!(filter_for_partials(crossing2, true, 10, 10).is_some());
}

#[test]
fn partials_false_filters_auto_candidates_per_slot() {
    let candidates = vec![
        PositionCandidate {
            anchor_offset_px: (0.0, -10.0),
            bbox_px: (-5.0, -10.0, 5.0, 0.0), // off-canvas top
        },
        PositionCandidate {
            anchor_offset_px: (0.0, 10.0),
            bbox_px: (10.0, 30.0, 20.0, 40.0), // fully inside
        },
    ];
    let placement = PreparedPlacement::Auto { candidates };
    let kept = filter_for_partials(placement, false, 100, 100).expect("one candidate fits");
    match kept {
        PreparedPlacement::Auto { candidates } => assert_eq!(candidates.len(), 1),
        _ => panic!("expected Auto"),
    }
}

#[test]
fn partials_false_drops_label_when_all_auto_candidates_off_canvas() {
    let candidates = vec![
        PositionCandidate {
            anchor_offset_px: (0.0, 0.0),
            bbox_px: (-5.0, -5.0, 5.0, 5.0),
        },
        PositionCandidate {
            anchor_offset_px: (0.0, 0.0),
            bbox_px: (95.0, 95.0, 105.0, 105.0),
        },
    ];
    let placement = PreparedPlacement::Auto { candidates };
    assert!(filter_for_partials(placement, false, 100, 100).is_none());
}

#[test]
fn follow_placement_emits_follow_label_drawop() {
    let label = PreparedLabel {
        raw_anchor_px: (50.0, 50.0),
        text: "ROAD".into(),
        style: Arc::new(ResolvedLabelStyle {
            font_family: String::new(),
            font_size: 12.0,
            fill: mars_style::Colour::rgba(0, 0, 0, 255),
            halo: None,
            priority: 10,
            min_distance: 0.0,
            position: AnchorPosition::default(),
            offset_px: (0.0, 0.0),
            angle_deg: None,
            partials: true,
            force: false,
        }),
        priority: 10,
        angle_rad: 0.0,
        placement: PreparedPlacement::Follow {
            polyline_px: vec![(0.0, 50.0), (100.0, 50.0)],
            start_arc_px: 25.0,
            bbox_px: (25.0, 45.0, 75.0, 55.0),
        },
    };
    let ops = collide_and_emit_labels(vec![label], 200, 200);
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        DrawOp::FollowLabel {
            polyline_px,
            start_arc_px,
            text,
            ..
        } => {
            assert_eq!(polyline_px.len(), 2);
            assert!((start_arc_px - 25.0).abs() < 1e-6);
            assert_eq!(text, "ROAD");
        }
        other => panic!("expected FollowLabel, got {other:?}"),
    }
}

#[test]
fn follow_placement_drops_on_collision_with_higher_priority() {
    // pre-occupy the bbox region with a high-priority axis label, then
    // a lower-priority Follow label aimed at the same bbox: the Follow
    // label drops.
    let blocker = prepared((25.0, 45.0, 75.0, 55.0), 100, 0.0);
    let follow = PreparedLabel {
        raw_anchor_px: (50.0, 50.0),
        text: "ROAD".into(),
        style: blocker.style.clone(),
        priority: 1,
        angle_rad: 0.0,
        placement: PreparedPlacement::Follow {
            polyline_px: vec![(0.0, 50.0), (100.0, 50.0)],
            start_arc_px: 25.0,
            bbox_px: (25.0, 45.0, 75.0, 55.0),
        },
    };
    let ops = collide_and_emit_labels(vec![blocker, follow], 200, 200);
    assert_eq!(ops.len(), 1, "Follow label collides with blocker and drops");
    assert!(matches!(ops[0], DrawOp::Label { .. }), "blocker survives");
}

#[test]
fn force_follow_bypasses_collision() {
    let blocker = prepared((25.0, 45.0, 75.0, 55.0), 100, 0.0);
    let mut style = (*blocker.style).clone();
    style.force = true;
    style.priority = 1;
    let follow = PreparedLabel {
        raw_anchor_px: (50.0, 50.0),
        text: "ROAD".into(),
        style: Arc::new(style),
        priority: 1,
        angle_rad: 0.0,
        placement: PreparedPlacement::Follow {
            polyline_px: vec![(0.0, 50.0), (100.0, 50.0)],
            start_arc_px: 25.0,
            bbox_px: (25.0, 45.0, 75.0, 55.0),
        },
    };
    let ops = collide_and_emit_labels(vec![blocker, follow], 200, 200);
    // both survive: forced Follow places ahead of blocker (force-first
    // sort), then blocker's high priority collides with it and drops.
    // verify the Follow is among the survivors.
    assert!(ops.iter().any(|op| matches!(op, DrawOp::FollowLabel { .. })));
}

#[test]
fn effective_angle_picks_style_override_when_set() {
    let mut s = ResolvedLabelStyle {
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

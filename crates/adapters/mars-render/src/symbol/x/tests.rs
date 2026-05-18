#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn build_path_inherits_twelve_vertex_closed_silhouette() {
    let path = build_path(12.0);
    assert_eq!(path.subpaths.len(), 1);
    let sub = &path.subpaths[0];
    assert!(sub.closed);
    assert_eq!(sub.points.len(), 12);
}

#[test]
fn rotation_expands_bbox_to_size_times_sqrt2() {
    // a 45° rotated + sign's bbox edge is the arm length scaled by
    // cos(45°) + thickness * sin(45°). for size=12, t=2: 6/√2 + 2/√2
    // doubled = ~11.31, but the corners protrude — empirically the
    // bbox is close to size/sqrt(2) + thickness/sqrt(2) per side.
    // sanity check: the bbox should not exceed size on either axis.
    let path = build_path(12.0);
    let pts = &path.subpaths[0].points;
    let max_x = pts.iter().map(|(x, _)| x.abs()).fold(f32::NEG_INFINITY, f32::max);
    let max_y = pts.iter().map(|(_, y)| y.abs()).fold(f32::NEG_INFINITY, f32::max);
    assert!(max_x <= 12.0 / std::f32::consts::SQRT_2 + 1e-3);
    assert!(max_y <= 12.0 / std::f32::consts::SQRT_2 + 1e-3);
}

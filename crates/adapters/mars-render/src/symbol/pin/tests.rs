#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn build_path_includes_apex_and_arc_vertices() {
    let path = build_path(10.0);
    assert_eq!(path.subpaths.len(), 1);
    let sub = &path.subpaths[0];
    assert!(sub.closed);
    // arc_segments + 1 arc-endpoint pair (closed) + 1 apex.
    assert_eq!(sub.points.len(), ARC_SEGMENTS + 2);
}

#[test]
fn apex_is_last_vertex_and_sits_below_bulb_centre() {
    let path = build_path(10.0);
    let pts = &path.subpaths[0].points;
    let apex = pts[pts.len() - 1];
    assert_eq!(apex.0, 0.0);
    assert!((apex.1 - 10.0).abs() < 1e-5);
}

#[test]
fn bbox_height_extends_to_apex() {
    let path = build_path(10.0);
    let pts = &path.subpaths[0].points;
    let min_y = pts.iter().map(|(_, y)| *y).fold(f32::INFINITY, f32::min);
    let max_y = pts.iter().map(|(_, y)| *y).fold(f32::NEG_INFINITY, f32::max);
    // bulb top reaches -r = -5; apex sits at +size = +10. height = 15.
    assert!((min_y - -5.0).abs() < 1e-4);
    assert!((max_y - 10.0).abs() < 1e-4);
}

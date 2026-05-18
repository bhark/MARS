#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn build_path_is_twelve_vertex_closed_polygon() {
    let path = build_path(12.0);
    assert_eq!(path.subpaths.len(), 1);
    let sub = &path.subpaths[0];
    assert!(sub.closed);
    assert_eq!(sub.points.len(), 12);
}

#[test]
fn bbox_matches_size() {
    let path = build_path(12.0);
    let pts = &path.subpaths[0].points;
    let min_x = pts.iter().map(|(x, _)| *x).fold(f32::INFINITY, f32::min);
    let max_x = pts.iter().map(|(x, _)| *x).fold(f32::NEG_INFINITY, f32::max);
    assert!((max_x - min_x - 12.0).abs() < 1e-5);
}

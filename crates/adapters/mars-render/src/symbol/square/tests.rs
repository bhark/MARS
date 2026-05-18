#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn build_path_yields_closed_quad() {
    let path = build_path(8.0);
    assert_eq!(path.subpaths.len(), 1);
    let sub = &path.subpaths[0];
    assert!(sub.closed);
    assert_eq!(sub.points.len(), 4);
}

#[test]
fn build_path_bbox_matches_size() {
    let path = build_path(10.0);
    let pts = &path.subpaths[0].points;
    let min = pts.iter().map(|(x, _)| *x).fold(f32::INFINITY, f32::min);
    let max = pts.iter().map(|(x, _)| *x).fold(f32::NEG_INFINITY, f32::max);
    assert!((max - min - 10.0).abs() < 1e-5);
}

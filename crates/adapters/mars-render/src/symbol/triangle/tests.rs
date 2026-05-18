#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn build_path_yields_closed_triangle() {
    let path = build_path(8.0);
    assert_eq!(path.subpaths.len(), 1);
    let sub = &path.subpaths[0];
    assert!(sub.closed);
    assert_eq!(sub.points.len(), 3);
}

#[test]
fn apex_above_base_in_raster_coords() {
    let path = build_path(10.0);
    let pts = &path.subpaths[0].points;
    // apex is the first vertex; it must sit above the other two.
    assert!(pts[0].1 < pts[1].1);
    assert!(pts[0].1 < pts[2].1);
}

#[test]
fn base_width_matches_size() {
    let path = build_path(12.0);
    let pts = &path.subpaths[0].points;
    let base_width = (pts[1].0 - pts[2].0).abs();
    assert!((base_width - 12.0).abs() < 1e-5);
}

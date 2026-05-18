#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use mars_render_port::Subpath;

fn build(points: Vec<(f32, f32)>, closed: bool) -> Option<tiny_skia::Path> {
    build_path(&PortPath {
        subpaths: vec![Subpath { points, closed }],
    })
}

#[test]
fn build_path_drops_subpath_with_single_point() {
    assert!(build(vec![(1.0, 2.0)], false).is_none());
}

#[test]
fn is_fillable_rejects_horizontal_line() {
    let p = build(vec![(0.0, 5.0), (10.0, 5.0)], false).expect("path");
    assert!(!is_fillable(&p));
}

#[test]
fn is_fillable_rejects_vertical_line() {
    let p = build(vec![(5.0, 0.0), (5.0, 10.0)], false).expect("path");
    assert!(!is_fillable(&p));
}

#[test]
fn is_fillable_rejects_collapsed_closed_polygon() {
    // closed ring whose vertices all share the same y - typical of a tiny
    // polygon flattened onto a pixel row by world->pixel projection.
    let p = build(vec![(0.0, 7.0), (4.0, 7.0), (8.0, 7.0)], true).expect("path");
    assert!(!is_fillable(&p));
}

#[test]
fn is_fillable_accepts_proper_polygon() {
    let p = build(vec![(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0)], true).expect("path");
    assert!(is_fillable(&p));
}

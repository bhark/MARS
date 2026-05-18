#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn anchor_at_origin_centres_unit_square() {
    let unit = vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
    let p = build_path(&unit, (0.5, 0.5), true, 10.0);
    let pts = &p.subpaths[0].points;
    // anchor (0.5, 0.5) maps to origin; unit square corners land at ±5.
    assert!((pts[0].0 - -5.0).abs() < 1e-5 && (pts[0].1 - -5.0).abs() < 1e-5);
    assert!((pts[2].0 - 5.0).abs() < 1e-5 && (pts[2].1 - 5.0).abs() < 1e-5);
}

#[test]
fn filled_true_yields_closed_subpath() {
    let unit = vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
    let p = build_path(&unit, (0.5, 0.5), true, 10.0);
    assert!(p.subpaths[0].closed);
}

#[test]
fn filled_false_yields_open_subpath() {
    let unit = vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)];
    let p = build_path(&unit, (0.5, 0.5), false, 10.0);
    assert!(!p.subpaths[0].closed);
}

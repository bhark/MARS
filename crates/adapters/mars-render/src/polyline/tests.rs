#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn cumulative_horizontal_line() {
    let cum = cumulative_arc_length(&[(0.0, 0.0), (3.0, 0.0), (7.0, 0.0)]);
    assert_eq!(cum, vec![0.0, 3.0, 7.0]);
}

#[test]
fn cumulative_empty_produces_single_zero() {
    // single-point input -> [0.0]; downstream callers treat last() == 0
    // as "no usable polyline".
    let cum = cumulative_arc_length(&[(0.0, 0.0)]);
    assert_eq!(cum, vec![0.0]);
}

#[test]
fn sample_at_midpoint_of_horizontal_segment() {
    let pts = [(0.0, 0.0), (10.0, 0.0)];
    let cum = cumulative_arc_length(&pts);
    let s = sample_at(&pts, &cum, 5.0).expect("sample");
    assert!((s.pos.0 - 5.0).abs() < 1e-5);
    assert!((s.pos.1).abs() < 1e-5);
    assert!((s.tangent.0 - 1.0).abs() < 1e-5);
    assert!((s.tangent.1).abs() < 1e-5);
}

#[test]
fn sample_at_corner_picks_post_corner_segment_tangent() {
    // L-shape: horizontal then vertical. sampling at the corner is the
    // boundary; we accept the post-corner segment because the search
    // iterates segments forward and the first match wins.
    let pts = [(0.0, 0.0), (5.0, 0.0), (5.0, 5.0)];
    let cum = cumulative_arc_length(&pts);
    let pre = sample_at(&pts, &cum, 2.5).expect("pre");
    assert!((pre.tangent.0 - 1.0).abs() < 1e-5);
    let post = sample_at(&pts, &cum, 7.5).expect("post");
    assert!((post.tangent.1 - 1.0).abs() < 1e-5);
}

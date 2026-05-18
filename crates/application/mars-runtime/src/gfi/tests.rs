#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn pixel_to_world_origin_matches_viewport_max_y() {
    let v = Bbox::new(0.0, 0.0, 100.0, 100.0);
    // (0, 0) pixel = top-left = (0, 100) world.
    let (x, y) = pixel_to_world((0, 0), v, 100, 100);
    assert!(x.abs() < 0.001);
    assert!((y - 100.0).abs() < 0.001);
}

#[test]
fn pixel_to_world_far_corner_matches_viewport_min_y() {
    let v = Bbox::new(0.0, 0.0, 100.0, 100.0);
    // (100, 100) pixel = bottom-right = (100, 0) world.
    let (x, y) = pixel_to_world((100, 100), v, 100, 100);
    assert!((x - 100.0).abs() < 0.001);
    assert!(y.abs() < 0.001);
}

#[test]
fn pixel_buffered_bbox_one_pixel_wide() {
    let v = Bbox::new(0.0, 0.0, 100.0, 100.0);
    let bb = pixel_buffered_bbox((50.0, 50.0), v, 100, 100);
    assert!((bb.width() - 1.0).abs() < 1e-6);
    assert!((bb.height() - 1.0).abs() < 1e-6);
}

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mars_style::{MarkerShape, ResolvedMarker};

use super::super::path_at;

#[test]
fn marker_vector_shape_uses_anchor_and_scale() {
    // unit-frame upward triangle, anchor at the bottom centre.
    let m = ResolvedMarker {
        shape: MarkerShape::VectorShape {
            points: vec![(0.0, 1.0), (1.0, 1.0), (0.5, 0.0)],
            anchor: (0.5, 1.0),
            filled: true,
        },
        size: 10.0,
        rotation_rad: None,
    };
    let p = path_at(&m, (100.0, 200.0));
    let sp = &p.subpaths[0];
    assert!(sp.closed);
    // anchor (0.5, 1.0) -> (100, 200). apex (0.5, 0.0) is 1 local-unit
    // above the anchor, so 10 px above in pixel space.
    let (apex_x, apex_y) = sp.points[2];
    assert!((apex_x - 100.0).abs() < 0.001);
    assert!((apex_y - 190.0).abs() < 0.001);
}

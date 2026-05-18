#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mars_style::{MarkerShape, ResolvedMarker};

use super::super::path_at;

#[test]
fn marker_triangle_has_three_vertices() {
    let p = path_at(
        &ResolvedMarker {
            shape: MarkerShape::Triangle,
            size: 12.0,
            rotation_rad: None,
        },
        (10.0, 10.0),
    );
    assert_eq!(p.subpaths[0].points.len(), 3);
    assert!(p.subpaths[0].closed);
}

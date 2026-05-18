#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mars_style::{MarkerShape, ResolvedMarker};

use super::super::{assert_marker_centred, path_at};

#[test]
fn marker_square_has_four_vertices_and_is_centred() {
    let p = path_at(
        &ResolvedMarker {
            shape: MarkerShape::Square,
            size: 8.0,
            rotation_rad: None,
        },
        (32.0, 16.0),
    );
    assert_marker_centred(&p, (32.0, 16.0), 8.0, 0.001);
    assert_eq!(p.subpaths[0].points.len(), 4);
}

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mars_style::{MarkerShape, ResolvedMarker};

use super::super::{assert_marker_centred, path_at};

#[test]
fn marker_cross_has_twelve_vertices() {
    let p = path_at(
        &ResolvedMarker {
            shape: MarkerShape::Cross,
            size: 12.0,
            rotation_rad: None,
        },
        (0.0, 0.0),
    );
    assert_eq!(p.subpaths[0].points.len(), 12);
    assert_marker_centred(&p, (0.0, 0.0), 12.0, 0.001);
}

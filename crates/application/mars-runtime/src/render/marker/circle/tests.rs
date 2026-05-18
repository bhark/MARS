#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mars_style::{MarkerShape, ResolvedMarker};

use super::super::{assert_marker_centred, path_at};

#[test]
fn marker_circle_is_closed_and_centred() {
    let p = path_at(
        &ResolvedMarker {
            shape: MarkerShape::Circle,
            size: 10.0,
            rotation_rad: None,
        },
        (50.0, 50.0),
    );
    assert_marker_centred(&p, (50.0, 50.0), 10.0, 0.5);
}

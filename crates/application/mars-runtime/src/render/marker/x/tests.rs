#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mars_style::{MarkerShape, ResolvedMarker};

use super::super::{bbox_of, path_at};

#[test]
fn marker_x_has_twelve_vertices() {
    let p = path_at(
        &ResolvedMarker {
            shape: MarkerShape::X,
            size: 12.0,
            rotation_rad: None,
        },
        (0.0, 0.0),
    );
    assert_eq!(p.subpaths[0].points.len(), 12);
    // X is a 45-degree rotation of the cross; symmetric around centre.
    let (minx, miny, maxx, maxy) = bbox_of(&p);
    let cx = (minx + maxx) * 0.5;
    let cy = (miny + maxy) * 0.5;
    assert!(cx.abs() < 0.5);
    assert!(cy.abs() < 0.5);
}

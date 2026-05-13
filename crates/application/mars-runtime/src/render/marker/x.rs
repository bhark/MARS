//! saltire (X) marker: the plus sign rotated 45 degrees.

use mars_render_port::{Path, Subpath};

pub(super) fn path(size: f32, (cx, cy): (f32, f32)) -> Path {
    let r = size * 0.5;
    let aw = size / 6.0;
    let cos45 = std::f32::consts::FRAC_1_SQRT_2;
    let rotate = |x: f32, y: f32| -> (f32, f32) {
        (
            cx + (x - cx) * cos45 - (y - cy) * cos45,
            cy + (x - cx) * cos45 + (y - cy) * cos45,
        )
    };
    let pts = [
        (cx - aw, cy - r),
        (cx + aw, cy - r),
        (cx + aw, cy - aw),
        (cx + r, cy - aw),
        (cx + r, cy + aw),
        (cx + aw, cy + aw),
        (cx + aw, cy + r),
        (cx - aw, cy + r),
        (cx - aw, cy + aw),
        (cx - r, cy + aw),
        (cx - r, cy - aw),
        (cx - aw, cy - aw),
    ];
    Path {
        subpaths: vec![Subpath {
            points: pts.iter().map(|&(x, y)| rotate(x, y)).collect(),
            closed: true,
        }],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use mars_style::MarkerSymbol;

    use super::super::{bbox_of, path_at};

    #[test]
    fn marker_x_has_twelve_vertices() {
        let p = path_at(&MarkerSymbol::X { size: 12.0 }, (0.0, 0.0));
        assert_eq!(p.subpaths[0].points.len(), 12);
        // X is a 45-degree rotation of the cross; symmetric around centre.
        let (minx, miny, maxx, maxy) = bbox_of(&p);
        let cx = (minx + maxx) * 0.5;
        let cy = (miny + maxy) * 0.5;
        assert!(cx.abs() < 0.5);
        assert!(cy.abs() < 0.5);
    }
}

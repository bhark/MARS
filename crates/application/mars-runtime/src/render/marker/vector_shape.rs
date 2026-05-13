//! arbitrary closed polygon described by a point list in a unit-square local
//! frame. mirrors mapserver SYMBOL TYPE VECTOR with explicit POINTS.
//!
//! local frame -> pixel: scale by size, translate so anchor maps to pos.
//! local-frame y is mapserver-y-down by convention; pixel space is also
//! y-down, so the sign is preserved.

use mars_render_port::{Path, Subpath};

pub(super) fn path(points: &[(f32, f32)], anchor: (f32, f32), filled: bool, size: f32, (cx, cy): (f32, f32)) -> Path {
    let (ax, ay) = anchor;
    let pts: Vec<(f32, f32)> = points
        .iter()
        .map(|(lx, ly)| (cx + (lx - ax) * size, cy + (ly - ay) * size))
        .collect();
    Path {
        subpaths: vec![Subpath {
            points: pts,
            closed: filled,
        }],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use mars_style::MarkerSymbol;

    use super::super::path_at;

    #[test]
    fn marker_vector_shape_uses_anchor_and_scale() {
        // unit-frame upward triangle, anchor at the bottom centre.
        let m = MarkerSymbol::VectorShape {
            points: vec![(0.0, 1.0), (1.0, 1.0), (0.5, 0.0)],
            anchor: (0.5, 1.0),
            filled: true,
            size: 10.0,
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
}

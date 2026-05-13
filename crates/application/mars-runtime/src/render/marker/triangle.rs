//! equilateral triangle marker, point up. circumradius = size/2.

use mars_render_port::{Path, Subpath};

pub(super) fn path(size: f32, (cx, cy): (f32, f32)) -> Path {
    let r = size * 0.5;
    let half_base = r * 0.866_025_4_f32;
    Path {
        subpaths: vec![Subpath {
            points: vec![
                (cx, cy - r),
                (cx + half_base, cy + r * 0.5),
                (cx - half_base, cy + r * 0.5),
            ],
            closed: true,
        }],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use mars_style::MarkerSymbol;

    use super::super::path_at;

    #[test]
    fn marker_triangle_has_three_vertices() {
        let p = path_at(&MarkerSymbol::Triangle { size: 12.0 }, (10.0, 10.0));
        assert_eq!(p.subpaths[0].points.len(), 3);
        assert!(p.subpaths[0].closed);
    }
}

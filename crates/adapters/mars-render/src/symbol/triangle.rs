//! equilateral triangle marker. `size` is the base edge length; the
//! triangle is centred on its centroid with the apex pointing up
//! (negative y in raster coords).

use mars_render_port::{Path as PortPath, Subpath};

pub(crate) fn build_path(size: f32) -> PortPath {
    // equilateral: total height = size * sqrt(3)/2; centroid sits 1/3 up
    // from the base, so apex_y = -2h/3 and base_y = +h/3.
    let s3 = 3f32.sqrt();
    let apex_y = -size * s3 / 3.0;
    let base_y = size * s3 / 6.0;
    let base_x = size / 2.0;
    PortPath {
        subpaths: vec![Subpath {
            points: vec![(0.0, apex_y), (base_x, base_y), (-base_x, base_y)],
            closed: true,
        }],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn build_path_yields_closed_triangle() {
        let path = build_path(8.0);
        assert_eq!(path.subpaths.len(), 1);
        let sub = &path.subpaths[0];
        assert!(sub.closed);
        assert_eq!(sub.points.len(), 3);
    }

    #[test]
    fn apex_above_base_in_raster_coords() {
        let path = build_path(10.0);
        let pts = &path.subpaths[0].points;
        // apex is the first vertex; it must sit above the other two.
        assert!(pts[0].1 < pts[1].1);
        assert!(pts[0].1 < pts[2].1);
    }

    #[test]
    fn base_width_matches_size() {
        let path = build_path(12.0);
        let pts = &path.subpaths[0].points;
        let base_width = (pts[1].0 - pts[2].0).abs();
        assert!((base_width - 12.0).abs() < 1e-5);
    }
}

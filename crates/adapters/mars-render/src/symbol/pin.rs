//! pin (teardrop) marker. `size` is the bulb diameter; the apex extends
//! straight down (positive y in raster coords) so the total height is
//! roughly `1.5 * size`. The silhouette is one closed polygon traced
//! along the bulb arc from the right tangent point down to the apex and
//! back up to the left tangent point.

use mars_render_port::{Path as PortPath, Subpath};

const ARC_SEGMENTS: usize = 24;

pub(crate) fn build_path(size: f32) -> PortPath {
    let r = size / 2.0;
    let apex = (0.0, size);
    // tangent points from (0, size) to circle of radius r at origin land
    // at y = r^2/h = size/4 (in y-down raster). arc to keep starts at
    // theta=30° (right tangent) and runs ccw through the top to theta=210°
    // (= -150°), spanning 240° of the bulb.
    let arc_start_deg = 30.0_f32;
    let arc_span_deg = 240.0_f32;
    let mut points: Vec<(f32, f32)> = Vec::with_capacity(ARC_SEGMENTS + 2);
    for i in 0..=ARC_SEGMENTS {
        let frac = (i as f32) / (ARC_SEGMENTS as f32);
        let theta_deg = arc_start_deg - frac * arc_span_deg;
        let theta = theta_deg.to_radians();
        points.push((r * theta.cos(), r * theta.sin()));
    }
    points.push(apex);
    PortPath {
        subpaths: vec![Subpath { points, closed: true }],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn build_path_includes_apex_and_arc_vertices() {
        let path = build_path(10.0);
        assert_eq!(path.subpaths.len(), 1);
        let sub = &path.subpaths[0];
        assert!(sub.closed);
        // arc_segments + 1 arc-endpoint pair (closed) + 1 apex.
        assert_eq!(sub.points.len(), ARC_SEGMENTS + 2);
    }

    #[test]
    fn apex_is_last_vertex_and_sits_below_bulb_centre() {
        let path = build_path(10.0);
        let pts = &path.subpaths[0].points;
        let apex = pts[pts.len() - 1];
        assert_eq!(apex.0, 0.0);
        assert!((apex.1 - 10.0).abs() < 1e-5);
    }

    #[test]
    fn bbox_height_extends_to_apex() {
        let path = build_path(10.0);
        let pts = &path.subpaths[0].points;
        let min_y = pts.iter().map(|(_, y)| *y).fold(f32::INFINITY, f32::min);
        let max_y = pts.iter().map(|(_, y)| *y).fold(f32::NEG_INFINITY, f32::max);
        // bulb top reaches -r = -5; apex sits at +size = +10. height = 15.
        assert!((min_y - -5.0).abs() < 1e-4);
        assert!((max_y - 10.0).abs() < 1e-4);
    }
}

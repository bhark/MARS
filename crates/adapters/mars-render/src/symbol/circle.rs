//! circle marker. `size` is the bounding-box edge (the bulb diameter);
//! the polygon is a 32-vertex regular approximation centred at the origin.

use mars_render_port::{Path as PortPath, Subpath};

const SEGMENTS: usize = 32;

pub(crate) fn build_path(size: f32) -> PortPath {
    let r = size / 2.0;
    let mut points = Vec::with_capacity(SEGMENTS);
    for i in 0..SEGMENTS {
        let theta = std::f32::consts::TAU * (i as f32) / (SEGMENTS as f32);
        points.push((r * theta.cos(), r * theta.sin()));
    }
    PortPath {
        subpaths: vec![Subpath { points, closed: true }],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn build_path_yields_closed_polygon_with_expected_vertex_count() {
        let path = build_path(8.0);
        assert_eq!(path.subpaths.len(), 1);
        let sub = &path.subpaths[0];
        assert!(sub.closed);
        assert_eq!(sub.points.len(), SEGMENTS);
    }

    #[test]
    fn build_path_bbox_matches_size() {
        let path = build_path(10.0);
        let pts = &path.subpaths[0].points;
        let min_x = pts.iter().map(|(x, _)| *x).fold(f32::INFINITY, f32::min);
        let max_x = pts.iter().map(|(x, _)| *x).fold(f32::NEG_INFINITY, f32::max);
        let min_y = pts.iter().map(|(_, y)| *y).fold(f32::INFINITY, f32::min);
        let max_y = pts.iter().map(|(_, y)| *y).fold(f32::NEG_INFINITY, f32::max);
        // polygon vertices sit on the circumscribed circle; bbox is exactly
        // the circle's diameter on both axes for the chosen segment count.
        assert!((max_x - min_x - 10.0).abs() < 1e-5);
        assert!((max_y - min_y - 10.0).abs() < 1e-5);
    }
}

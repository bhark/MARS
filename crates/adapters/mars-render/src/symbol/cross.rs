//! cross (+) marker. `size` is the arm length end-to-end; arm thickness
//! is `size/3` (mapserver SYMBOL TYPE VECTOR cross convention). One
//! closed 12-vertex polygon so the even-odd fill rule paints the whole
//! silhouette in a single subpath without hole artefacts.

use mars_render_port::{Path as PortPath, Subpath};

pub(crate) fn build_path(size: f32) -> PortPath {
    let half = size / 2.0;
    let t = size / 6.0; // arm half-thickness
    PortPath {
        subpaths: vec![Subpath {
            // traverse the + silhouette ccw starting at the top-arm's top-left
            points: vec![
                (-t, -half),
                (t, -half),
                (t, -t),
                (half, -t),
                (half, t),
                (t, t),
                (t, half),
                (-t, half),
                (-t, t),
                (-half, t),
                (-half, -t),
                (-t, -t),
            ],
            closed: true,
        }],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn build_path_is_twelve_vertex_closed_polygon() {
        let path = build_path(12.0);
        assert_eq!(path.subpaths.len(), 1);
        let sub = &path.subpaths[0];
        assert!(sub.closed);
        assert_eq!(sub.points.len(), 12);
    }

    #[test]
    fn bbox_matches_size() {
        let path = build_path(12.0);
        let pts = &path.subpaths[0].points;
        let min_x = pts.iter().map(|(x, _)| *x).fold(f32::INFINITY, f32::min);
        let max_x = pts.iter().map(|(x, _)| *x).fold(f32::NEG_INFINITY, f32::max);
        assert!((max_x - min_x - 12.0).abs() < 1e-5);
    }
}

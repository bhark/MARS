//! user-supplied polygon marker. `points` lives in a normalised
//! `[0, 1] × [0, 1]` local frame (mapserver SYMBOL TYPE VECTOR POINTS
//! convention); `anchor` is the local-frame point that maps to the
//! feature anchor, and `size` is the pixel edge length of the unit
//! square. `filled` selects polygon (closed) vs. polyline (open)
//! semantics; the dispatch hub clears `Style::fill` for the open case so
//! the existing fill pipeline is bypassed cleanly.

use mars_render_port::{Path as PortPath, Subpath};

pub(crate) fn build_path(points: &[(f32, f32)], anchor: (f32, f32), filled: bool, size: f32) -> PortPath {
    let transformed: Vec<(f32, f32)> = points
        .iter()
        .map(|(x, y)| ((x - anchor.0) * size, (y - anchor.1) * size))
        .collect();
    PortPath {
        subpaths: vec![Subpath {
            points: transformed,
            closed: filled,
        }],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn anchor_at_origin_centres_unit_square() {
        let unit = vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
        let p = build_path(&unit, (0.5, 0.5), true, 10.0);
        let pts = &p.subpaths[0].points;
        // anchor (0.5, 0.5) maps to origin; unit square corners land at ±5.
        assert!((pts[0].0 - -5.0).abs() < 1e-5 && (pts[0].1 - -5.0).abs() < 1e-5);
        assert!((pts[2].0 - 5.0).abs() < 1e-5 && (pts[2].1 - 5.0).abs() < 1e-5);
    }

    #[test]
    fn filled_true_yields_closed_subpath() {
        let unit = vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
        let p = build_path(&unit, (0.5, 0.5), true, 10.0);
        assert!(p.subpaths[0].closed);
    }

    #[test]
    fn filled_false_yields_open_subpath() {
        let unit = vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)];
        let p = build_path(&unit, (0.5, 0.5), false, 10.0);
        assert!(!p.subpaths[0].closed);
    }
}

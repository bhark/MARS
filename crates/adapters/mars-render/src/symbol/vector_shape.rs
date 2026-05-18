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
mod tests;

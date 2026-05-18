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
mod tests;

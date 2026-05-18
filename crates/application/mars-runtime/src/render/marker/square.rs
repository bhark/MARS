//! axis-aligned square marker.

use mars_render_port::{Path, Subpath};

pub(super) fn path(size: f32, (cx, cy): (f32, f32)) -> Path {
    let r = size * 0.5;
    Path {
        subpaths: vec![Subpath {
            points: vec![(cx - r, cy - r), (cx + r, cy - r), (cx + r, cy + r), (cx - r, cy + r)],
            closed: true,
        }],
    }
}

#[cfg(test)]
mod tests;

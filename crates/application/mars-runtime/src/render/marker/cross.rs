//! plus-sign marker. arm half-width = size/6, visually balanced.

use mars_render_port::{Path, Subpath};

pub(super) fn path(size: f32, (cx, cy): (f32, f32)) -> Path {
    let r = size * 0.5;
    let aw = size / 6.0;
    Path {
        subpaths: vec![Subpath {
            points: vec![
                (cx - aw, cy - r),
                (cx + aw, cy - r),
                (cx + aw, cy - aw),
                (cx + r, cy - aw),
                (cx + r, cy + aw),
                (cx + aw, cy + aw),
                (cx + aw, cy + r),
                (cx - aw, cy + r),
                (cx - aw, cy + aw),
                (cx - r, cy + aw),
                (cx - r, cy - aw),
                (cx - aw, cy - aw),
            ],
            closed: true,
        }],
    }
}

#[cfg(test)]
mod tests;

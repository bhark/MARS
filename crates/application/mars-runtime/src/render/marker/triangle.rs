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
mod tests;

//! saltire (X) marker: the plus sign rotated 45 degrees.

use mars_render_port::{Path, Subpath};

pub(super) fn path(size: f32, (cx, cy): (f32, f32)) -> Path {
    let r = size * 0.5;
    let aw = size / 6.0;
    let cos45 = std::f32::consts::FRAC_1_SQRT_2;
    let rotate = |x: f32, y: f32| -> (f32, f32) {
        (
            cx + (x - cx) * cos45 - (y - cy) * cos45,
            cy + (x - cx) * cos45 + (y - cy) * cos45,
        )
    };
    let pts = [
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
    ];
    Path {
        subpaths: vec![Subpath {
            points: pts.iter().map(|&(x, y)| rotate(x, y)).collect(),
            closed: true,
        }],
    }
}

#[cfg(test)]
mod tests;

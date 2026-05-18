//! map pin (teardrop): a bulb circle of radius r centred 1.4*r above the
//! tip. tangents from the tip touch the bulb at +/- asin(r / 1.4r) from
//! vertical; the arc sweeps the long way over the top of the bulb between
//! the two tangent points, then a single segment closes back to the tip.
//!
//! the geometric anchor is the tip (`pos`), not the visual centre, so pins
//! look like map pins with the bulb above the anchor point.

use mars_render_port::{Path, Subpath};

pub(super) fn path(size: f32, (cx, cy): (f32, f32)) -> Path {
    const N: usize = 22;
    let r = size * 0.5;
    let dy = r * 1.4;
    let bulb_cy = cy - dy;
    let alpha = (r / dy).asin();
    let start = std::f32::consts::FRAC_PI_2 + alpha;
    let end = std::f32::consts::FRAC_PI_2 - alpha + std::f32::consts::TAU;
    let mut pts: Vec<(f32, f32)> = (0..=N)
        .map(|i| {
            let t = i as f32 / N as f32;
            let theta = start + (end - start) * t;
            (cx + r * theta.cos(), bulb_cy + r * theta.sin())
        })
        .collect();
    pts.push((cx, cy));
    Path {
        subpaths: vec![Subpath {
            points: pts,
            closed: true,
        }],
    }
}

#[cfg(test)]
mod tests;

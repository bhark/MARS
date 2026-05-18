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
mod tests;

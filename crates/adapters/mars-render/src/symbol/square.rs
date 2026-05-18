//! square marker. `size` is the edge length; the polygon is centred at
//! the origin so corners sit at `±size/2`.

use mars_render_port::{Path as PortPath, Subpath};

pub(crate) fn build_path(size: f32) -> PortPath {
    let h = size / 2.0;
    PortPath {
        subpaths: vec![Subpath {
            points: vec![(-h, -h), (h, -h), (h, h), (-h, h)],
            closed: true,
        }],
    }
}

#[cfg(test)]
mod tests;

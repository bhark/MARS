//! equilateral triangle marker. `size` is the base edge length; the
//! triangle is centred on its centroid with the apex pointing up
//! (negative y in raster coords).

use mars_render_port::{Path as PortPath, Subpath};

pub(crate) fn build_path(size: f32) -> PortPath {
    // equilateral: total height = size * sqrt(3)/2; centroid sits 1/3 up
    // from the base, so apex_y = -2h/3 and base_y = +h/3.
    let s3 = 3f32.sqrt();
    let apex_y = -size * s3 / 3.0;
    let base_y = size * s3 / 6.0;
    let base_x = size / 2.0;
    PortPath {
        subpaths: vec![Subpath {
            points: vec![(0.0, apex_y), (base_x, base_y), (-base_x, base_y)],
            closed: true,
        }],
    }
}

#[cfg(test)]
mod tests;

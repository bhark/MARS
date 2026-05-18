//! point geometry: reproject one coord, build a marker-aware subpath.

use mars_render_port::Subpath;
use mars_style::ResolvedMarker;
use mars_types::Bbox;

use crate::RuntimeError;
use crate::render::map_proj_err;

pub(super) fn project(c: (f64, f64), xform: &mars_proj::Transformer) -> Result<(f64, f64), RuntimeError> {
    xform.transform_point(c.0, c.1).map_err(map_proj_err)
}

pub(super) fn subpaths(c: (f64, f64), viewport: Bbox, w: u32, h: u32, marker: Option<&ResolvedMarker>) -> Vec<Subpath> {
    let pos = super::world_to_pixel(c, viewport, w, h);
    match marker {
        Some(m) => crate::render::marker::path_at(m, pos).subpaths,
        None => vec![Subpath {
            points: vec![pos],
            closed: false,
        }],
    }
}

#[cfg(test)]
mod tests;

//! linestring geometry: reproject one ring, build one open subpath.

use mars_render_port::Subpath;
use mars_types::Bbox;

use crate::RuntimeError;

pub(super) fn project(coords: &[(f64, f64)], xform: &mars_proj::Transformer) -> Result<Vec<(f64, f64)>, RuntimeError> {
    super::project_ring(coords, xform)
}

pub(super) fn subpaths(coords: &[(f64, f64)], viewport: Bbox, w: u32, h: u32) -> Vec<Subpath> {
    vec![super::ring_to_subpath(coords, viewport, w, h, false)]
}

#[cfg(test)]
mod tests;

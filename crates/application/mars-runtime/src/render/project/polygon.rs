//! polygon geometry: reproject rings, build one closed subpath per ring.

use mars_render_port::Subpath;
use mars_types::Bbox;

use crate::RuntimeError;

pub(super) fn project(
    rings: &[Vec<(f64, f64)>],
    xform: &mars_proj::Transformer,
) -> Result<Vec<Vec<(f64, f64)>>, RuntimeError> {
    let mut out = Vec::with_capacity(rings.len());
    for ring in rings {
        out.push(super::project_ring(ring, xform)?);
    }
    Ok(out)
}

pub(super) fn subpaths(rings: &[Vec<(f64, f64)>], viewport: Bbox, w: u32, h: u32) -> Vec<Subpath> {
    rings
        .iter()
        .map(|r| super::ring_to_subpath(r, viewport, w, h, true))
        .collect()
}

#[cfg(test)]
mod tests;

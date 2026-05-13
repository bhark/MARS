//! multilinestring geometry: reproject parts, build one open subpath per part.

use mars_render_port::Subpath;
use mars_types::Bbox;

use crate::RuntimeError;

pub(super) fn project(
    parts: &[Vec<(f64, f64)>],
    xform: &mars_proj::Transformer,
) -> Result<Vec<Vec<(f64, f64)>>, RuntimeError> {
    let mut out = Vec::with_capacity(parts.len());
    for ring in parts {
        out.push(super::project_ring(ring, xform)?);
    }
    Ok(out)
}

pub(super) fn subpaths(parts: &[Vec<(f64, f64)>], viewport: Bbox, w: u32, h: u32) -> Vec<Subpath> {
    parts
        .iter()
        .map(|r| super::ring_to_subpath(r, viewport, w, h, false))
        .collect()
}

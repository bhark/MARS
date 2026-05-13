//! multipolygon geometry: reproject nested rings, build one closed subpath
//! per ring across all polygons.

use mars_render_port::Subpath;
use mars_types::Bbox;

use crate::RuntimeError;

#[allow(clippy::type_complexity)]
pub(super) fn project(
    parts: &[Vec<Vec<(f64, f64)>>],
    xform: &mars_proj::Transformer,
) -> Result<Vec<Vec<Vec<(f64, f64)>>>, RuntimeError> {
    let mut out = Vec::with_capacity(parts.len());
    for poly in parts {
        let mut rings = Vec::with_capacity(poly.len());
        for ring in poly {
            rings.push(super::project_ring(ring, xform)?);
        }
        out.push(rings);
    }
    Ok(out)
}

pub(super) fn subpaths(parts: &[Vec<Vec<(f64, f64)>>], viewport: Bbox, w: u32, h: u32) -> Vec<Subpath> {
    parts
        .iter()
        .flat_map(|poly| poly.iter().map(|r| super::ring_to_subpath(r, viewport, w, h, true)))
        .collect()
}

//! multipoint geometry: reproject as a flat coord list, build marker-aware
//! subpaths (one per point).

use mars_render_port::Subpath;
use mars_style::ResolvedMarker;
use mars_types::Bbox;

use crate::RuntimeError;

pub(super) fn project(coords: &[(f64, f64)], xform: &mars_proj::Transformer) -> Result<Vec<(f64, f64)>, RuntimeError> {
    super::project_ring(coords, xform)
}

pub(super) fn subpaths(
    coords: &[(f64, f64)],
    viewport: Bbox,
    w: u32,
    h: u32,
    marker: Option<&ResolvedMarker>,
) -> Vec<Subpath> {
    match marker {
        Some(m) => coords
            .iter()
            .flat_map(|&c| crate::render::marker::path_at(m, super::world_to_pixel(c, viewport, w, h)).subpaths)
            .collect(),
        None => coords
            .iter()
            .map(|&c| Subpath {
                points: vec![super::world_to_pixel(c, viewport, w, h)],
                closed: false,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests;

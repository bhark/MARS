//! multipoint geometry: reproject as a flat coord list, build marker-aware
//! subpaths (one per point).

use mars_render_port::Subpath;
use mars_style::MarkerSymbol;
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
    marker: Option<&MarkerSymbol>,
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use mars_artifact::GeomKind;
    use mars_render_port::DrawOp;
    use mars_style::{MarkerShape, MarkerSymbol, Style};
    use mars_types::Bbox;

    use crate::render::project::feature_to_drawop;

    #[test]
    fn feature_to_drawop_multipoint_marker_emits_one_subpath_per_point() {
        let geom = GeomKind::MultiPoint(vec![(2.0, 2.0), (5.0, 5.0), (8.0, 8.0)]);
        let v = Bbox::new(0.0, 0.0, 10.0, 10.0);
        let style = Arc::new(Style {
            marker: Some(MarkerSymbol {
                shape: MarkerShape::Square,
                size: 6.0,
            }),
            ..Default::default()
        });
        let op = feature_to_drawop(&geom, v, 100, 100, style).unwrap();
        let DrawOp::Path { path, .. } = op else {
            panic!("expected path");
        };
        assert_eq!(path.subpaths.len(), 3);
        for sp in &path.subpaths {
            assert!(sp.closed);
            assert_eq!(sp.points.len(), 4);
        }
    }
}

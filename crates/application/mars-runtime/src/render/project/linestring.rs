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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use mars_artifact::GeomKind;
    use mars_render_port::DrawOp;
    use mars_style::{Colour, FillPaint, ResolvedStyle, Style};
    use mars_types::Bbox;

    use crate::render::project::feature_to_drawop;

    fn test_style() -> Arc<ResolvedStyle> {
        Arc::new(
            Style {
                fill: Some(FillPaint::Solid(Colour {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 255,
                })),
                ..Default::default()
            }
            .resolve(0),
        )
    }

    #[test]
    fn feature_to_drawop_linestring_open() {
        let geom = GeomKind::LineString(vec![(0.0, 0.0), (10.0, 10.0)]);
        let v = Bbox::new(0.0, 0.0, 10.0, 10.0);
        let op = feature_to_drawop(&geom, v, 100, 100, test_style()).unwrap();
        if let DrawOp::Path { path, .. } = op {
            assert_eq!(path.subpaths.len(), 1);
            assert!(!path.subpaths[0].closed);
        } else {
            panic!("expected DrawOp::Path");
        }
    }
}

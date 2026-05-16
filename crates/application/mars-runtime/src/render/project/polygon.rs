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
    fn feature_to_drawop_polygon_rings_closed() {
        let geom = GeomKind::Polygon(vec![vec![
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
            (0.0, 0.0),
        ]]);
        let v = Bbox::new(0.0, 0.0, 10.0, 10.0);
        let op = feature_to_drawop(&geom, v, 100, 100, test_style()).unwrap();
        match op {
            DrawOp::Path { path, .. } => {
                assert_eq!(path.subpaths.len(), 1);
                assert!(path.subpaths[0].closed);
                assert_eq!(path.subpaths[0].points.len(), 5);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}

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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use mars_artifact::GeomKind;
    use mars_render_port::DrawOp;
    use mars_style::{Colour, FillPaint, MarkerShape, MarkerSymbol, ResolvedStyle, Style};
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
    fn feature_to_drawop_point_uses_marker_when_set() {
        let geom = GeomKind::Point((5.0, 5.0));
        let v = Bbox::new(0.0, 0.0, 10.0, 10.0);
        let style = Arc::new(
            Style {
                fill: Some(FillPaint::Solid(Colour::rgba(0, 0, 0, 255))),
                marker: Some(MarkerSymbol {
                    shape: MarkerShape::Circle,
                    size: 12.0.into(),
                    angle: None,
                }),
                ..Default::default()
            }
            .resolve(0),
        );
        let op = feature_to_drawop(&geom, v, 100, 100, style).unwrap();
        let DrawOp::Path { path, .. } = op else {
            panic!("expected path");
        };
        // a marker emits a closed circle with N=24 vertices, not a single
        // anchor point.
        assert_eq!(path.subpaths.len(), 1);
        assert!(path.subpaths[0].closed);
        assert!(path.subpaths[0].points.len() >= 12);
    }

    #[test]
    fn feature_to_drawop_point_without_marker_emits_single_anchor() {
        let geom = GeomKind::Point((5.0, 5.0));
        let v = Bbox::new(0.0, 0.0, 10.0, 10.0);
        let op = feature_to_drawop(&geom, v, 100, 100, test_style()).unwrap();
        let DrawOp::Path { path, .. } = op else {
            panic!("expected path");
        };
        assert_eq!(path.subpaths.len(), 1);
        assert!(!path.subpaths[0].closed);
        assert_eq!(path.subpaths[0].points.len(), 1);
    }
}

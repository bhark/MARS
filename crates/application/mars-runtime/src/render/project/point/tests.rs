#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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

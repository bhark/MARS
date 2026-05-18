#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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
    let style = Arc::new(
        Style {
            marker: Some(MarkerSymbol {
                shape: MarkerShape::Square,
                size: 6.0.into(),
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
    assert_eq!(path.subpaths.len(), 3);
    for sp in &path.subpaths {
        assert!(sp.closed);
        assert_eq!(sp.points.len(), 4);
    }
}

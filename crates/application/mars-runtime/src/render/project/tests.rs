#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn world_to_pixel_origin_top_left() {
    let v = Bbox::new(0.0, 0.0, 10.0, 10.0);
    let (px, py) = world_to_pixel((0.0, 10.0), v, 100, 100);
    assert!(px.abs() < 0.001);
    assert!(py.abs() < 0.001);
}

#[test]
fn world_to_pixel_far_corner_bottom_right() {
    let v = Bbox::new(0.0, 0.0, 10.0, 10.0);
    let (px, py) = world_to_pixel((10.0, 0.0), v, 100, 100);
    assert!((px - 100.0).abs() < 0.001);
    assert!((py - 100.0).abs() < 0.001);
}

#[test]
fn world_to_pixel_clamps_degenerate_viewport() {
    let v = Bbox::new(0.0, 0.0, 0.0, 0.0);
    assert_eq!(world_to_pixel((1.0, 1.0), v, 10, 10), (0.0, 0.0));
}

use mars_style::{Colour, FillPaint, GeomTransform, MarkerShape, MarkerSymbol, Style};

fn marker_style(t: GeomTransform) -> Arc<ResolvedStyle> {
    Arc::new(
        Style {
            fill: Some(FillPaint::Solid(Colour::rgba(0, 0, 0, 0xff))),
            marker: Some(MarkerSymbol {
                shape: MarkerShape::Square,
                size: 4.0.into(),
                angle: None,
            }),
            geom_transform: Some(t),
            ..Default::default()
        }
        .resolve(0),
    )
}

#[test]
fn geom_transform_vertices_on_linestring_stamps_marker_per_vertex() {
    let g = GeomKind::LineString(vec![(0.0, 0.0), (5.0, 5.0), (10.0, 10.0)]);
    let v = Bbox::new(0.0, 0.0, 10.0, 10.0);
    let op = feature_to_drawop(&g, v, 100, 100, marker_style(GeomTransform::Vertices)).unwrap();
    let DrawOp::Path { path, .. } = op else {
        panic!("expected path");
    };
    // one closed square subpath per vertex.
    assert_eq!(path.subpaths.len(), 3);
    for sp in &path.subpaths {
        assert!(sp.closed);
    }
}

#[test]
fn geom_transform_start_on_linestring_yields_one_marker() {
    let g = GeomKind::LineString(vec![(0.0, 0.0), (5.0, 5.0), (10.0, 10.0)]);
    let v = Bbox::new(0.0, 0.0, 10.0, 10.0);
    let op = feature_to_drawop(&g, v, 100, 100, marker_style(GeomTransform::Start)).unwrap();
    let DrawOp::Path { path, .. } = op else {
        panic!("expected path");
    };
    assert_eq!(path.subpaths.len(), 1);
}

#[test]
fn geom_transform_end_on_linestring_yields_one_marker() {
    let g = GeomKind::LineString(vec![(0.0, 0.0), (5.0, 5.0), (10.0, 10.0)]);
    let v = Bbox::new(0.0, 0.0, 10.0, 10.0);
    let op = feature_to_drawop(&g, v, 100, 100, marker_style(GeomTransform::End)).unwrap();
    let DrawOp::Path { path, .. } = op else {
        panic!("expected path");
    };
    assert_eq!(path.subpaths.len(), 1);
}

#[test]
fn geom_transform_vertices_on_polygon_flattens_rings() {
    let g = GeomKind::Polygon(vec![vec![
        (0.0, 0.0),
        (10.0, 0.0),
        (10.0, 10.0),
        (0.0, 10.0),
        (0.0, 0.0),
    ]]);
    let v = Bbox::new(0.0, 0.0, 10.0, 10.0);
    let op = feature_to_drawop(&g, v, 100, 100, marker_style(GeomTransform::Vertices)).unwrap();
    let DrawOp::Path { path, .. } = op else {
        panic!("expected path");
    };
    // 5 ring coords -> 5 marker subpaths.
    assert_eq!(path.subpaths.len(), 5);
}

#[test]
fn geom_transform_returns_none_on_empty_geometry() {
    let g = GeomKind::LineString(vec![]);
    let v = Bbox::new(0.0, 0.0, 10.0, 10.0);
    assert!(feature_to_drawop(&g, v, 100, 100, marker_style(GeomTransform::Start)).is_none());
}

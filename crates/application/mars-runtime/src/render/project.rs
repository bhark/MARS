//! coord transforms and pixel mapping used by the render path.
//!
//! pure functions only: bbox reprojection, per-feature reprojection, and
//! world->pixel mapping with the `feature_to_drawop` adapter that turns a
//! decoded `GeomKind` into a `DrawOp::Path`.

use std::sync::Arc;

use mars_artifact::{FeatureGeom, GeomKind};
use mars_render_port::{DrawOp, Path, Subpath};
use mars_style::Style;
use mars_types::Bbox;

use crate::RuntimeError;
use crate::plan as planning;

use super::map_proj_err;

pub(super) fn bbox_native(
    viewport: Bbox,
    from: &mars_types::CrsCode,
    to: &mars_types::CrsCode,
) -> Result<Bbox, RuntimeError> {
    planning::reproject_viewport(viewport, from, to)
}

pub(super) fn bbox_to_f32(b: Bbox) -> [f32; 4] {
    [b.min_x as f32, b.min_y as f32, b.max_x as f32, b.max_y as f32]
}

pub(super) fn project_paired_features(
    features: Vec<(u32, FeatureGeom)>,
    from: &mars_types::CrsCode,
    to: &mars_types::CrsCode,
) -> Result<Vec<(u32, FeatureGeom)>, RuntimeError> {
    let xform = mars_proj::cached_transformer(from, to).map_err(map_proj_err)?;
    let mut out = Vec::with_capacity(features.len());
    for (slot, f) in features {
        let geom = project_geom(&f.geom, &xform)?;
        out.push((
            slot,
            FeatureGeom {
                user_id: f.user_id,
                bbox: f.bbox,
                geom,
            },
        ));
    }
    Ok(out)
}

fn project_geom(g: &GeomKind, xform: &mars_proj::Transformer) -> Result<GeomKind, RuntimeError> {
    let mapped = match g {
        GeomKind::Point(c) => GeomKind::Point(project_point(*c, xform)?),
        GeomKind::LineString(coords) => GeomKind::LineString(project_ring(coords, xform)?),
        GeomKind::Polygon(rings) => {
            let mut out = Vec::with_capacity(rings.len());
            for ring in rings {
                out.push(project_ring(ring, xform)?);
            }
            GeomKind::Polygon(out)
        }
        GeomKind::MultiPoint(coords) => GeomKind::MultiPoint(project_ring(coords, xform)?),
        GeomKind::MultiLineString(parts) => {
            let mut out = Vec::with_capacity(parts.len());
            for ring in parts {
                out.push(project_ring(ring, xform)?);
            }
            GeomKind::MultiLineString(out)
        }
        GeomKind::MultiPolygon(parts) => {
            let mut out = Vec::with_capacity(parts.len());
            for poly in parts {
                let mut rings = Vec::with_capacity(poly.len());
                for ring in poly {
                    rings.push(project_ring(ring, xform)?);
                }
                out.push(rings);
            }
            GeomKind::MultiPolygon(out)
        }
    };
    Ok(mapped)
}

fn project_point(c: (f64, f64), xform: &mars_proj::Transformer) -> Result<(f64, f64), RuntimeError> {
    xform.transform_point(c.0, c.1).map_err(map_proj_err)
}

fn project_ring(coords: &[(f64, f64)], xform: &mars_proj::Transformer) -> Result<Vec<(f64, f64)>, RuntimeError> {
    if coords.is_empty() {
        return Ok(Vec::new());
    }
    let mut buf: Vec<[f64; 2]> = coords.iter().map(|&(x, y)| [x, y]).collect();
    xform.transform_points(&mut buf).map_err(map_proj_err)?;
    Ok(buf.into_iter().map(|p| (p[0], p[1])).collect())
}

pub(super) fn feature_to_drawop(g: &GeomKind, viewport: Bbox, w: u32, h: u32, style: Arc<Style>) -> Option<DrawOp> {
    let path = match g {
        GeomKind::Point(c) => single_point_path(*c, viewport, w, h),
        GeomKind::LineString(coords) => Path {
            subpaths: vec![ring_to_subpath(coords, viewport, w, h, false)],
        },
        GeomKind::Polygon(rings) => Path {
            subpaths: rings.iter().map(|r| ring_to_subpath(r, viewport, w, h, true)).collect(),
        },
        GeomKind::MultiPoint(coords) => Path {
            subpaths: coords
                .iter()
                .map(|&c| Subpath {
                    points: vec![world_to_pixel(c, viewport, w, h)],
                    closed: false,
                })
                .collect(),
        },
        GeomKind::MultiLineString(parts) => Path {
            subpaths: parts
                .iter()
                .map(|r| ring_to_subpath(r, viewport, w, h, false))
                .collect(),
        },
        GeomKind::MultiPolygon(parts) => Path {
            subpaths: parts
                .iter()
                .flat_map(|poly| poly.iter().map(|r| ring_to_subpath(r, viewport, w, h, true)))
                .collect(),
        },
    };
    if path.subpaths.is_empty() {
        return None;
    }
    Some(DrawOp::Path { path, style })
}

fn ring_to_subpath(coords: &[(f64, f64)], viewport: Bbox, w: u32, h: u32, closed: bool) -> Subpath {
    Subpath {
        points: coords.iter().map(|&c| world_to_pixel(c, viewport, w, h)).collect(),
        closed,
    }
}

fn single_point_path(c: (f64, f64), viewport: Bbox, w: u32, h: u32) -> Path {
    Path {
        subpaths: vec![Subpath {
            points: vec![world_to_pixel(c, viewport, w, h)],
            closed: false,
        }],
    }
}

pub(super) fn world_to_pixel(c: (f64, f64), viewport: Bbox, w: u32, h: u32) -> (f32, f32) {
    let dx = viewport.width();
    let dy = viewport.height();
    if !dx.is_finite() || !dy.is_finite() || dx <= 0.0 || dy <= 0.0 {
        return (0.0, 0.0);
    }
    let nx = (c.0 - viewport.min_x) / dx;
    let ny = (c.1 - viewport.min_y) / dy;
    let px = nx * f64::from(w);
    let py = (1.0 - ny) * f64::from(h);
    (px as f32, py as f32)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use mars_style::{Colour, FillPaint};

    use super::*;

    fn test_style() -> Arc<Style> {
        Arc::new(Style {
            fill: Some(FillPaint::Solid(Colour {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            })),
            stroke: None,
            stroke_width: None,
            stroke_dasharray: None,
            stroke_linecap: None,
            stroke_linejoin: None,
            marker: None,
        })
    }

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

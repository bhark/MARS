//! coord transforms and pixel mapping used by the render path.
//!
//! pure functions only: bbox reprojection, per-feature reprojection, and
//! world->pixel mapping with the `feature_to_drawop` adapter that turns a
//! decoded `GeomKind` into a `DrawOp::Path`.
//!
//! `GeomKind` is the canonical vocabulary at this seam. each variant has
//! its own module under `project/`; this hub holds the dispatches and
//! the helpers that all variants share (`world_to_pixel`, `project_ring`,
//! `ring_to_subpath`). adding a `GeomKind` variant breaks the build at
//! the two match arms below and forces a new file alongside.

mod geom_transform;
mod linestring;
mod multilinestring;
mod multipoint;
mod multipolygon;
mod point;
mod polygon;

use std::sync::Arc;

use mars_artifact::{FeatureGeom, GeomKind};
use mars_render_port::{DrawOp, Path, Subpath};
use mars_style::ResolvedStyle;
use mars_types::Bbox;

use crate::RuntimeError;
use crate::plan as planning;
use crate::render::map_proj_err;

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
        GeomKind::Point(c) => GeomKind::Point(point::project(*c, xform)?),
        GeomKind::LineString(coords) => GeomKind::LineString(linestring::project(coords, xform)?),
        GeomKind::Polygon(rings) => GeomKind::Polygon(polygon::project(rings, xform)?),
        GeomKind::MultiPoint(coords) => GeomKind::MultiPoint(multipoint::project(coords, xform)?),
        GeomKind::MultiLineString(parts) => GeomKind::MultiLineString(multilinestring::project(parts, xform)?),
        GeomKind::MultiPolygon(parts) => GeomKind::MultiPolygon(multipolygon::project(parts, xform)?),
    };
    Ok(mapped)
}

pub(super) fn feature_to_drawop(
    g: &GeomKind,
    viewport: Bbox,
    w: u32,
    h: u32,
    style: Arc<ResolvedStyle>,
) -> Option<DrawOp> {
    // geom_transform short-circuits the per-kind dispatch: we derive a
    // synthetic point set from the input and route through multipoint::subpaths
    // so the existing marker pipeline stamps each derived position.
    let subpaths: Vec<Subpath> = if let Some(t) = style.geom_transform {
        let pts = geom_transform::derived_points(g, t);
        multipoint::subpaths(&pts, viewport, w, h, style.marker.as_ref())
    } else {
        match g {
            GeomKind::Point(c) => point::subpaths(*c, viewport, w, h, style.marker.as_ref()),
            GeomKind::LineString(coords) => linestring::subpaths(coords, viewport, w, h),
            GeomKind::Polygon(rings) => polygon::subpaths(rings, viewport, w, h),
            GeomKind::MultiPoint(coords) => multipoint::subpaths(coords, viewport, w, h, style.marker.as_ref()),
            GeomKind::MultiLineString(parts) => multilinestring::subpaths(parts, viewport, w, h),
            GeomKind::MultiPolygon(parts) => multipolygon::subpaths(parts, viewport, w, h),
        }
    };
    if subpaths.is_empty() {
        return None;
    }
    Some(DrawOp::Path {
        path: Path { subpaths },
        style,
    })
}

fn project_ring(coords: &[(f64, f64)], xform: &mars_proj::Transformer) -> Result<Vec<(f64, f64)>, RuntimeError> {
    if coords.is_empty() {
        return Ok(Vec::new());
    }
    let mut buf: Vec<[f64; 2]> = coords.iter().map(|&(x, y)| [x, y]).collect();
    xform.transform_points(&mut buf).map_err(map_proj_err)?;
    Ok(buf.into_iter().map(|p| (p[0], p[1])).collect())
}

fn ring_to_subpath(coords: &[(f64, f64)], viewport: Bbox, w: u32, h: u32, closed: bool) -> Subpath {
    Subpath {
        points: coords.iter().map(|&c| world_to_pixel(c, viewport, w, h)).collect(),
        closed,
    }
}

/// Longer pixel-space side of `geom`'s axis-aligned bbox. Returns `0.0` for
/// the degenerate viewport. Used by the per-pass `MINFEATURESIZE` gate, so
/// the bbox is taken from the geometry's actual vertices rather than the
/// stored feature bbox (which may be stale post-reproject).
pub(super) fn pixel_extent(geom: &GeomKind, viewport: Bbox, w: u32, h: u32) -> f32 {
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    let mut visit = |c: (f64, f64)| {
        let (px, py) = world_to_pixel(c, viewport, w, h);
        if px < min_x {
            min_x = px;
        }
        if py < min_y {
            min_y = py;
        }
        if px > max_x {
            max_x = px;
        }
        if py > max_y {
            max_y = py;
        }
    };
    walk_coords(geom, &mut visit);
    if !(min_x.is_finite() && max_x.is_finite()) {
        return 0.0;
    }
    (max_x - min_x).max(max_y - min_y)
}

fn walk_coords(g: &GeomKind, visit: &mut impl FnMut((f64, f64))) {
    match g {
        GeomKind::Point(c) => visit(*c),
        GeomKind::LineString(coords) | GeomKind::MultiPoint(coords) => coords.iter().copied().for_each(visit),
        GeomKind::Polygon(rings) => {
            for ring in rings {
                ring.iter().copied().for_each(&mut *visit);
            }
        }
        GeomKind::MultiLineString(parts) => {
            for part in parts {
                part.iter().copied().for_each(&mut *visit);
            }
        }
        GeomKind::MultiPolygon(parts) => {
            for poly in parts {
                for ring in poly {
                    ring.iter().copied().for_each(&mut *visit);
                }
            }
        }
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
}

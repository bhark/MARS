//! coord transforms and pixel mapping used by the render path.
//!
//! pure functions only: bbox reprojection, per-feature reprojection, and
//! world->pixel mapping with the `feature_to_drawop` adapter that turns a
//! decoded `GeomKind` into a `DrawOp::Path`.

use std::sync::Arc;

use mars_artifact::{FeatureGeom, GeomKind};
use mars_render_port::{DrawOp, Path, Subpath};
use mars_style::{MarkerSymbol, Style};
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
        // point geometries with a marker symbol tessellate to the marker
        // shape at the projected pixel position; without a marker we emit a
        // zero-extent path (caller's stroke arm may still draw, but no fill).
        GeomKind::Point(c) => match style.marker.as_ref() {
            Some(m) => marker_path_at(m, world_to_pixel(*c, viewport, w, h)),
            None => single_point_path(*c, viewport, w, h),
        },
        GeomKind::LineString(coords) => Path {
            subpaths: vec![ring_to_subpath(coords, viewport, w, h, false)],
        },
        GeomKind::Polygon(rings) => Path {
            subpaths: rings.iter().map(|r| ring_to_subpath(r, viewport, w, h, true)).collect(),
        },
        GeomKind::MultiPoint(coords) => match style.marker.as_ref() {
            Some(m) => Path {
                subpaths: coords
                    .iter()
                    .flat_map(|&c| marker_path_at(m, world_to_pixel(c, viewport, w, h)).subpaths)
                    .collect(),
            },
            None => Path {
                subpaths: coords
                    .iter()
                    .map(|&c| Subpath {
                        points: vec![world_to_pixel(c, viewport, w, h)],
                        closed: false,
                    })
                    .collect(),
            },
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

/// tessellate a `MarkerSymbol` to a closed `Path` centred at `pos` in pixel
/// space. shapes are drawn outline-and-fill-friendly (closed subpaths) so
/// the renderer's `draw_path` fills and strokes per the enclosing `Style`.
///
/// pin is a teardrop (circle bulb + triangle tail) - it's the only marker
/// where the visual centre is *not* `pos`; we anchor the tip at `pos` so
/// pins look like map pins, with the bulb above.
pub(super) fn marker_path_at(m: &MarkerSymbol, pos: (f32, f32)) -> Path {
    let (cx, cy) = pos;
    let s = m.size();
    let r = s * 0.5;
    match m {
        MarkerSymbol::Circle { .. } => {
            // an N-segment polygon approximates a circle. 24 keeps it smooth
            // up to ~32 px without bloating the path.
            const N: usize = 24;
            let pts: Vec<(f32, f32)> = (0..N)
                .map(|i| {
                    let theta = (i as f32) * std::f32::consts::TAU / N as f32;
                    (cx + r * theta.cos(), cy + r * theta.sin())
                })
                .collect();
            Path {
                subpaths: vec![Subpath { points: pts, closed: true }],
            }
        }
        MarkerSymbol::Square { .. } => Path {
            subpaths: vec![Subpath {
                points: vec![
                    (cx - r, cy - r),
                    (cx + r, cy - r),
                    (cx + r, cy + r),
                    (cx - r, cy + r),
                ],
                closed: true,
            }],
        },
        MarkerSymbol::Triangle { .. } => {
            // equilateral triangle, point up. circumradius = r.
            let half_base = r * 0.866_025_4_f32;
            Path {
                subpaths: vec![Subpath {
                    points: vec![(cx, cy - r), (cx + half_base, cy + r * 0.5), (cx - half_base, cy + r * 0.5)],
                    closed: true,
                }],
            }
        }
        MarkerSymbol::Cross { .. } => {
            // a plus sign. arm half-width = s/6; visually balanced.
            let aw = s / 6.0;
            Path {
                subpaths: vec![Subpath {
                    points: vec![
                        (cx - aw, cy - r),
                        (cx + aw, cy - r),
                        (cx + aw, cy - aw),
                        (cx + r, cy - aw),
                        (cx + r, cy + aw),
                        (cx + aw, cy + aw),
                        (cx + aw, cy + r),
                        (cx - aw, cy + r),
                        (cx - aw, cy + aw),
                        (cx - r, cy + aw),
                        (cx - r, cy - aw),
                        (cx - aw, cy - aw),
                    ],
                    closed: true,
                }],
            }
        }
        MarkerSymbol::X { .. } => {
            // a saltire (X). diagonal arm half-width = s/6.
            let aw = s / 6.0;
            // outer corner extents along each diagonal; the arm runs from
            // (-r, -r+aw*sqrt2) through (0,0) etc. for simplicity we
            // describe two crossing strokes as a single 12-vertex polygon
            // by rotating the Cross by 45 degrees.
            let cos45 = std::f32::consts::FRAC_1_SQRT_2;
            let rotate = |x: f32, y: f32| -> (f32, f32) {
                (cx + (x - cx) * cos45 - (y - cy) * cos45, cy + (x - cx) * cos45 + (y - cy) * cos45)
            };
            let pts = [
                (cx - aw, cy - r),
                (cx + aw, cy - r),
                (cx + aw, cy - aw),
                (cx + r, cy - aw),
                (cx + r, cy + aw),
                (cx + aw, cy + aw),
                (cx + aw, cy + r),
                (cx - aw, cy + r),
                (cx - aw, cy + aw),
                (cx - r, cy + aw),
                (cx - r, cy - aw),
                (cx - aw, cy - aw),
            ];
            Path {
                subpaths: vec![Subpath {
                    points: pts.iter().map(|&(x, y)| rotate(x, y)).collect(),
                    closed: true,
                }],
            }
        }
        MarkerSymbol::Pin { .. } => {
            // teardrop. bulb circle of radius r centred 1.4*r above the tip
            // (the geometric anchor at `pos`). tangents from the tip touch
            // the bulb at +/- asin(r / 1.4r) from vertical; the arc sweeps
            // the long way over the top of the bulb between the two tangent
            // points, then a single segment closes back to the tip.
            const N: usize = 22;
            let dy = r * 1.4;
            let bulb_cy = cy - dy;
            let alpha = (r / dy).asin();
            let start = std::f32::consts::FRAC_PI_2 + alpha;
            let end = std::f32::consts::FRAC_PI_2 - alpha + std::f32::consts::TAU;
            let mut pts: Vec<(f32, f32)> = (0..=N)
                .map(|i| {
                    let t = i as f32 / N as f32;
                    let theta = start + (end - start) * t;
                    (cx + r * theta.cos(), bulb_cy + r * theta.sin())
                })
                .collect();
            pts.push((cx, cy));
            Path {
                subpaths: vec![Subpath { points: pts, closed: true }],
            }
        }
        MarkerSymbol::VectorShape {
            points,
            anchor,
            filled,
            size,
        } => {
            // local frame -> pixel: scale by size, translate so anchor maps
            // to pos. local-frame y is mapserver-y-down by convention, so the
            // sign is preserved as-is (pixel space is also y-down).
            let (ax, ay) = *anchor;
            let s = *size;
            let pts: Vec<(f32, f32)> = points
                .iter()
                .map(|(lx, ly)| (cx + (lx - ax) * s, cy + (ly - ay) * s))
                .collect();
            Path {
                subpaths: vec![Subpath {
                    points: pts,
                    closed: *filled,
                }],
            }
        }
        // glyph markers paint via the font path in raster::draw_path; emit a
        // single-anchor subpath so the renderer can stamp at the point.
        MarkerSymbol::Glyph { .. } => Path {
            subpaths: vec![Subpath {
                points: vec![(cx, cy)],
                closed: false,
            }],
        },
        // future MarkerSymbol variants land additively. fall back to a
        // zero-extent single-point path so the renderer's stroke arm still
        // runs but no marker shape is drawn.
        _ => Path {
            subpaths: vec![Subpath {
                points: vec![(cx, cy)],
                closed: false,
            }],
        },
    }
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
            ..Default::default()
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

    fn bbox_of(path: &Path) -> (f32, f32, f32, f32) {
        let mut minx = f32::INFINITY;
        let mut miny = f32::INFINITY;
        let mut maxx = f32::NEG_INFINITY;
        let mut maxy = f32::NEG_INFINITY;
        for sp in &path.subpaths {
            for &(x, y) in &sp.points {
                if x < minx {
                    minx = x;
                }
                if y < miny {
                    miny = y;
                }
                if x > maxx {
                    maxx = x;
                }
                if y > maxy {
                    maxy = y;
                }
            }
        }
        (minx, miny, maxx, maxy)
    }

    fn assert_marker_centred(path: &Path, pos: (f32, f32), expected_extent: f32, tol: f32) {
        assert!(!path.subpaths.is_empty(), "empty path");
        for sp in &path.subpaths {
            assert!(sp.closed, "marker subpath must be closed");
        }
        let (minx, miny, maxx, maxy) = bbox_of(path);
        let cx = (minx + maxx) * 0.5;
        let cy = (miny + maxy) * 0.5;
        let w = maxx - minx;
        let h = maxy - miny;
        assert!((cx - pos.0).abs() < tol, "x centre off: {cx} vs {}", pos.0);
        assert!((cy - pos.1).abs() < tol, "y centre off: {cy} vs {}", pos.1);
        assert!(
            (w - expected_extent).abs() < tol && (h - expected_extent).abs() < tol,
            "extent {w}x{h} != {expected_extent}",
        );
    }

    #[test]
    fn marker_circle_is_closed_and_centred() {
        let p = marker_path_at(&MarkerSymbol::Circle { size: 10.0 }, (50.0, 50.0));
        assert_marker_centred(&p, (50.0, 50.0), 10.0, 0.5);
    }

    #[test]
    fn marker_square_has_four_vertices_and_is_centred() {
        let p = marker_path_at(&MarkerSymbol::Square { size: 8.0 }, (32.0, 16.0));
        assert_marker_centred(&p, (32.0, 16.0), 8.0, 0.001);
        assert_eq!(p.subpaths[0].points.len(), 4);
    }

    #[test]
    fn marker_triangle_has_three_vertices() {
        let p = marker_path_at(&MarkerSymbol::Triangle { size: 12.0 }, (10.0, 10.0));
        assert_eq!(p.subpaths[0].points.len(), 3);
        assert!(p.subpaths[0].closed);
    }

    #[test]
    fn marker_cross_has_twelve_vertices() {
        let p = marker_path_at(&MarkerSymbol::Cross { size: 12.0 }, (0.0, 0.0));
        assert_eq!(p.subpaths[0].points.len(), 12);
        assert_marker_centred(&p, (0.0, 0.0), 12.0, 0.001);
    }

    #[test]
    fn marker_x_has_twelve_vertices() {
        let p = marker_path_at(&MarkerSymbol::X { size: 12.0 }, (0.0, 0.0));
        assert_eq!(p.subpaths[0].points.len(), 12);
        // X is a 45-degree rotation of the cross; symmetric around centre.
        let (minx, miny, maxx, maxy) = bbox_of(&p);
        let cx = (minx + maxx) * 0.5;
        let cy = (miny + maxy) * 0.5;
        assert!(cx.abs() < 0.5);
        assert!(cy.abs() < 0.5);
    }

    #[test]
    fn marker_vector_shape_uses_anchor_and_scale() {
        // unit-frame upward triangle, anchor at the bottom centre.
        let m = MarkerSymbol::VectorShape {
            points: vec![(0.0, 1.0), (1.0, 1.0), (0.5, 0.0)],
            anchor: (0.5, 1.0),
            filled: true,
            size: 10.0,
        };
        let p = marker_path_at(&m, (100.0, 200.0));
        let sp = &p.subpaths[0];
        assert!(sp.closed);
        // anchor (0.5, 1.0) -> (100, 200). apex (0.5, 0.0) is 1 local-unit
        // above the anchor, so 10 px above in pixel space.
        let (apex_x, apex_y) = sp.points[2];
        assert!((apex_x - 100.0).abs() < 0.001);
        assert!((apex_y - 190.0).abs() < 0.001);
    }

    #[test]
    fn marker_pin_tip_is_at_anchor_bulb_above() {
        let pos = (10.0, 100.0);
        let p = marker_path_at(&MarkerSymbol::Pin { size: 8.0 }, pos);
        assert!(p.subpaths[0].closed);
        let (_, miny, _, maxy) = bbox_of(&p);
        // tip at pos.1 = 100; bulb extends upward (smaller y in pixel space).
        assert!((maxy - 100.0).abs() < 0.5, "pin tip not at anchor: maxy={maxy}");
        assert!(miny < 100.0 - 4.0, "pin bulb not above tip: miny={miny}");
    }

    #[test]
    fn feature_to_drawop_point_uses_marker_when_set() {
        let geom = GeomKind::Point((5.0, 5.0));
        let v = Bbox::new(0.0, 0.0, 10.0, 10.0);
        let style = Arc::new(Style {
            fill: Some(FillPaint::Solid(Colour::rgba(0, 0, 0, 255))),
            marker: Some(MarkerSymbol::Circle { size: 12.0 }),
            ..Default::default()
        });
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

    #[test]
    fn feature_to_drawop_multipoint_marker_emits_one_subpath_per_point() {
        let geom = GeomKind::MultiPoint(vec![(2.0, 2.0), (5.0, 5.0), (8.0, 8.0)]);
        let v = Bbox::new(0.0, 0.0, 10.0, 10.0);
        let style = Arc::new(Style {
            marker: Some(MarkerSymbol::Square { size: 6.0 }),
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

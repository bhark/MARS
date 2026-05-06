//! per-cell draw-op emission: decode source + layer artifacts, merge-walk by
//! feature_id, viewport bbox filter, world→pixel transform, push DrawOp::Path.

use std::sync::Arc;

use mars_artifact::{
    ArtifactReader, FeatureGeom, GeomKind, SectionKind, decode_class_assignment, decode_geometry_payload,
    decode_style_refs,
};
use mars_proj::Transformer;
use mars_render_port::{DrawOp, Path, Subpath};
use mars_style::{Style, Stylesheet};
use mars_types::Bbox;

use crate::RuntimeError;

/// optional canonical-to-request reprojector applied to vertices before pixel
/// projection. `None` means the request is already in canonical CRS.
pub(crate) type Reproject<'a> = Option<&'a Transformer>;

/// request-space → pixel linear transform for a viewport.
///
/// `bbox` is in the **request** CRS — the same frame as `width` x `height`.
/// vertices in canonical CRS must be reprojected (see `Reproject`) before
/// pixel mapping.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Viewport {
    pub bbox: Bbox,
    pub width: u32,
    pub height: u32,
}

impl Viewport {
    fn project(self, x: f64, y: f64) -> (f32, f32) {
        let w = self.bbox.width();
        let h = self.bbox.height();
        let px = (x - self.bbox.min_x) / w * f64::from(self.width);
        // invert Y: world y grows up, pixel y grows down
        let py = (self.bbox.max_y - y) / h * f64::from(self.height);
        (px as f32, py as f32)
    }
}

#[inline]
pub(crate) fn bbox_intersects(feat: [f32; 4], vp: Bbox) -> bool {
    let (fx0, fy0, fx1, fy1) = (
        f64::from(feat[0]),
        f64::from(feat[1]),
        f64::from(feat[2]),
        f64::from(feat[3]),
    );
    !(fx1 < vp.min_x || fx0 > vp.max_x || fy1 < vp.min_y || fy0 > vp.max_y)
}

/// emit draw ops for one (source artifact, layer artifact) pair into `out`.
///
/// `canonical_bbox` is the request bbox expressed in the canonical CRS,
/// used to cull feature bboxes (which are stored in canonical coords).
/// `reproject`, when present, transforms vertices from canonical CRS into
/// the viewport's request CRS before pixel mapping.
pub(crate) fn emit_layer_cell(
    source: &ArtifactReader,
    layer: &ArtifactReader,
    stylesheet: &Stylesheet,
    viewport: Viewport,
    canonical_bbox: Bbox,
    reproject: Reproject<'_>,
    out: &mut Vec<DrawOp>,
) -> Result<(), RuntimeError> {
    if viewport.bbox.width() == 0.0 || viewport.bbox.height() == 0.0 {
        // defensive: zero-area viewport produces no draw ops. upstream parsers
        // (wms) already reject this, but non-wms callers may construct a plan
        // directly.
        return Ok(());
    }
    let geom_section = source.section(SectionKind::GeometryPayload)?;
    let features = decode_geometry_payload(&geom_section)?;

    let class_section = layer.section(SectionKind::ClassAssignment)?;
    let class_assignment = decode_class_assignment(&class_section)?;

    let style_refs_section = layer.section(SectionKind::StyleRefs)?;
    let style_refs = decode_style_refs(&style_refs_section)?;

    // merge-walk: features sorted by id ASC, class_assignment sorted by id ASC.
    let mut ai = 0usize;
    for feat in &features {
        while ai < class_assignment.len() && class_assignment[ai].0 < feat.id {
            ai += 1;
        }
        if ai >= class_assignment.len() || class_assignment[ai].0 != feat.id {
            continue;
        }
        let class_index = class_assignment[ai].1 as usize;
        ai += 1;

        if !bbox_intersects(feat.bbox, canonical_bbox) {
            continue;
        }

        let style_id = match style_refs.get(class_index) {
            Some(s) => s,
            None => {
                tracing::debug!(
                    "class_index {class_index} out of style_refs range ({})",
                    style_refs.len()
                );
                continue;
            }
        };
        let style = match stylesheet.geometry.get(style_id) {
            Some(s) => s.clone(),
            None => {
                tracing::debug!("style '{style_id}' missing from stylesheet");
                continue;
            }
        };

        if let Some(op) = build_draw_op(feat, viewport, reproject, &style)? {
            out.push(op);
        }
    }
    Ok(())
}

fn build_draw_op(
    feat: &FeatureGeom,
    vp: Viewport,
    reproject: Reproject<'_>,
    style: &Arc<Style>,
) -> Result<Option<DrawOp>, RuntimeError> {
    let subpaths = match &feat.geom {
        GeomKind::Polygon(rs) => project_polygon(rs, vp, reproject)?,
        GeomKind::MultiPolygon(parts) => {
            let mut acc = Vec::new();
            for rs in parts {
                acc.extend(project_polygon(rs, vp, reproject)?);
            }
            acc
        }
        GeomKind::LineString(verts) => vec![project_ring(verts, vp, reproject, false)?],
        GeomKind::MultiLineString(parts) => {
            let mut acc = Vec::with_capacity(parts.len());
            for p in parts {
                acc.push(project_ring(p, vp, reproject, false)?);
            }
            acc
        }
        // phase 0: points only meaningful with a fill; emit a 1px square. skip
        // if no fill style is configured.
        GeomKind::Point((x, y)) => {
            if style.fill.is_none() {
                return Ok(None);
            }
            vec![project_point_square(*x, *y, vp, reproject)?]
        }
        GeomKind::MultiPoint(pts) => {
            if style.fill.is_none() {
                return Ok(None);
            }
            let mut acc = Vec::with_capacity(pts.len());
            for (x, y) in pts {
                acc.push(project_point_square(*x, *y, vp, reproject)?);
            }
            acc
        }
    };
    if subpaths.is_empty() {
        return Ok(None);
    }
    Ok(Some(DrawOp::Path {
        path: Path { subpaths },
        style: style.clone(),
    }))
}

fn project_polygon(
    rings: &[Vec<(f64, f64)>],
    vp: Viewport,
    reproject: Reproject<'_>,
) -> Result<Vec<Subpath>, RuntimeError> {
    let mut out = Vec::with_capacity(rings.len());
    for r in rings {
        out.push(project_ring(r, vp, reproject, true)?);
    }
    Ok(out)
}

pub(crate) fn project_ring(
    verts: &[(f64, f64)],
    vp: Viewport,
    reproject: Reproject<'_>,
    close: bool,
) -> Result<Subpath, RuntimeError> {
    let mut points: Vec<(f32, f32)> = Vec::with_capacity(verts.len() + usize::from(close));
    for &(x, y) in verts {
        let (rx, ry) = reproject_point(x, y, reproject)?;
        points.push(vp.project(rx, ry));
    }
    if close && points.len() >= 2 && points[0] != points[points.len() - 1] {
        points.push(points[0]);
    }
    Ok(Subpath { points, closed: close })
}

pub(crate) fn project_point_square(
    x: f64,
    y: f64,
    vp: Viewport,
    reproject: Reproject<'_>,
) -> Result<Subpath, RuntimeError> {
    let (rx, ry) = reproject_point(x, y, reproject)?;
    let (px, py) = vp.project(rx, ry);
    Ok(Subpath {
        points: vec![(px, py), (px + 1.0, py), (px + 1.0, py + 1.0), (px, py + 1.0), (px, py)],
        closed: true,
    })
}

#[inline]
fn reproject_point(x: f64, y: f64, reproject: Reproject<'_>) -> Result<(f64, f64), RuntimeError> {
    match reproject {
        Some(t) => Ok(t.transform_point(x, y)?),
        None => Ok((x, y)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use mars_artifact::{ArtifactKind, ArtifactWriter, FeatureGeom, GeomKind};
    use mars_style::{Colour, Style, Stylesheet};
    use mars_types::Bbox;

    use super::*;

    fn viewport(bbox: Bbox, width: u32, height: u32) -> Viewport {
        Viewport { bbox, width, height }
    }

    #[test]
    fn project_origin() {
        let vp = viewport(Bbox::new(0.0, 0.0, 10.0, 10.0), 100, 100);
        assert_eq!(vp.project(0.0, 0.0), (0.0, 100.0));
        assert_eq!(vp.project(10.0, 10.0), (100.0, 0.0));
        assert_eq!(vp.project(5.0, 5.0), (50.0, 50.0));
    }

    #[test]
    fn project_negative_coords() {
        let vp = viewport(Bbox::new(-10.0, -10.0, 0.0, 0.0), 100, 100);
        assert_eq!(vp.project(-10.0, -10.0), (0.0, 100.0));
        assert_eq!(vp.project(0.0, 0.0), (100.0, 0.0));
    }

    #[test]
    fn bbox_intersects_touching_edges() {
        let vp = Bbox::new(0.0, 0.0, 10.0, 10.0);
        // feature exactly on left edge → included (fx1 == vp.min_x, not fx1 < vp.min_x)
        assert!(bbox_intersects([0.0, 0.0, 0.0, 10.0], vp));
        // feature exactly on right edge → included
        assert!(bbox_intersects([10.0, 0.0, 10.0, 10.0], vp));
        // feature just outside → excluded
        assert!(!bbox_intersects([10.1, 0.0, 11.0, 10.0], vp));
        assert!(!bbox_intersects([-1.0, 0.0, -0.1, 10.0], vp));
    }

    #[test]
    fn project_ring_closes_polygon() {
        let vp = viewport(Bbox::new(0.0, 0.0, 10.0, 10.0), 100, 100);
        let ring = vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)];
        let out = project_ring(&ring, vp, None, true).unwrap();
        assert_eq!(
            out.points[0],
            out.points[out.points.len() - 1],
            "polygon ring should be closed"
        );
        assert!(out.closed);
    }

    #[test]
    fn project_ring_does_not_close_linestring() {
        let vp = viewport(Bbox::new(0.0, 0.0, 10.0, 10.0), 100, 100);
        let ring = vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)];
        let out = project_ring(&ring, vp, None, false).unwrap();
        assert_ne!(
            out.points[0],
            out.points[out.points.len() - 1],
            "linestring should stay open"
        );
        assert!(!out.closed);
    }

    #[test]
    fn project_point_square_1px() {
        let vp = viewport(Bbox::new(0.0, 0.0, 10.0, 10.0), 100, 100);
        let sq = project_point_square(5.0, 5.0, vp, None).unwrap();
        assert_eq!(sq.points.len(), 5);
        assert_eq!(sq.points[0], sq.points[4]);
        assert_eq!(sq.points[1].0, sq.points[0].0 + 1.0);
        assert!(sq.closed);
    }

    fn build_source(features: &[FeatureGeom]) -> ArtifactReader {
        let mut w = ArtifactWriter::new(ArtifactKind::Source);
        w.add_geometry_payload(features)
            .set_bbox(Bbox::new(0.0, 0.0, 10.0, 10.0))
            .set_feature_count(features.len() as u64);
        ArtifactReader::open(w.finish().unwrap()).unwrap()
    }

    fn build_layer(class_assignment: &[(u64, u16)], style_refs: &[String]) -> ArtifactReader {
        let mut w = ArtifactWriter::new(ArtifactKind::Layer);
        w.add_class_assignment(class_assignment)
            .add_style_refs(style_refs)
            .set_bbox(Bbox::new(0.0, 0.0, 10.0, 10.0))
            .set_feature_count(class_assignment.len() as u64);
        ArtifactReader::open(w.finish().unwrap()).unwrap()
    }

    fn red_style() -> Style {
        Style {
            fill: Some(Colour {
                r: 255,
                g: 0,
                b: 0,
                a: 255,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn zero_area_viewport_returns_empty() {
        let src = build_source(&[FeatureGeom {
            id: 1,
            bbox: [0.0, 0.0, 10.0, 10.0],
            geom: GeomKind::Point((5.0, 5.0)),
        }]);
        let lyr = build_layer(&[(1, 0)], &["red".into()]);
        let mut ss = Stylesheet::default();
        ss.geometry.insert("red".into(), Arc::new(red_style()));
        let mut out = Vec::new();
        let vp = viewport(Bbox::new(0.0, 0.0, 0.0, 10.0), 100, 100);
        emit_layer_cell(&src, &lyr, &ss, vp, vp.bbox, None, &mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn out_of_range_style_ref_skips_feature() {
        let src = build_source(&[FeatureGeom {
            id: 1,
            bbox: [0.0, 0.0, 10.0, 10.0],
            geom: GeomKind::Point((5.0, 5.0)),
        }]);
        // class_index 5 but only 1 style_ref → out of range
        let lyr = build_layer(&[(1, 5)], &["red".into()]);
        let mut ss = Stylesheet::default();
        ss.geometry.insert("red".into(), Arc::new(red_style()));
        let mut out = Vec::new();
        let vp = viewport(Bbox::new(0.0, 0.0, 10.0, 10.0), 100, 100);
        emit_layer_cell(&src, &lyr, &ss, vp, vp.bbox, None, &mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn missing_style_in_stylesheet_skips_feature() {
        let src = build_source(&[FeatureGeom {
            id: 1,
            bbox: [0.0, 0.0, 10.0, 10.0],
            geom: GeomKind::Point((5.0, 5.0)),
        }]);
        let lyr = build_layer(&[(1, 0)], &["blue".into()]);
        let ss = Stylesheet::default(); // no "blue" style
        let mut out = Vec::new();
        let vp = viewport(Bbox::new(0.0, 0.0, 10.0, 10.0), 100, 100);
        emit_layer_cell(&src, &lyr, &ss, vp, vp.bbox, None, &mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn point_without_fill_is_skipped() {
        let src = build_source(&[FeatureGeom {
            id: 1,
            bbox: [0.0, 0.0, 10.0, 10.0],
            geom: GeomKind::Point((5.0, 5.0)),
        }]);
        let lyr = build_layer(&[(1, 0)], &["no_fill".into()]);
        let mut ss = Stylesheet::default();
        ss.geometry.insert("no_fill".into(), Arc::new(Style::default()));
        let mut out = Vec::new();
        let vp = viewport(Bbox::new(0.0, 0.0, 10.0, 10.0), 100, 100);
        emit_layer_cell(&src, &lyr, &ss, vp, vp.bbox, None, &mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn point_with_fill_emits_square() {
        let src = build_source(&[FeatureGeom {
            id: 1,
            bbox: [0.0, 0.0, 10.0, 10.0],
            geom: GeomKind::Point((5.0, 5.0)),
        }]);
        let lyr = build_layer(&[(1, 0)], &["red".into()]);
        let mut ss = Stylesheet::default();
        ss.geometry.insert("red".into(), Arc::new(red_style()));
        let mut out = Vec::new();
        let vp = viewport(Bbox::new(0.0, 0.0, 10.0, 10.0), 100, 100);
        emit_layer_cell(&src, &lyr, &ss, vp, vp.bbox, None, &mut out).unwrap();
        assert_eq!(out.len(), 1);
        match &out[0] {
            DrawOp::Path { path, .. } => {
                assert_eq!(path.subpaths.len(), 1);
                assert_eq!(path.subpaths[0].points.len(), 5); // 4 corners + close
                assert!(path.subpaths[0].closed);
            }
            other => panic!("expected Path, got {other:?}"),
        }
    }
}

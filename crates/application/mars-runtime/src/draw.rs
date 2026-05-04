//! per-cell draw-op emission: decode source + layer artifacts, merge-walk by
//! feature_id, viewport bbox filter, world→pixel transform, push DrawOp::Path.

use mars_artifact::{
    ArtifactReader, FeatureGeom, GeomKind, SectionKind, decode_class_assignment, decode_geometry_payload,
    decode_style_refs,
};
use mars_render_port::{DrawOp, Path};
use mars_style::{Style, Stylesheet};
use mars_types::Bbox;

use crate::RuntimeError;

/// world → pixel linear transform for a viewport.
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
pub(crate) fn emit_layer_cell(
    source: &ArtifactReader,
    layer: &ArtifactReader,
    stylesheet: &Stylesheet,
    viewport: Viewport,
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

        if !bbox_intersects(feat.bbox, viewport.bbox) {
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

        if let Some(op) = build_draw_op(feat, viewport, &style) {
            out.push(op);
        }
    }
    Ok(())
}

fn build_draw_op(feat: &FeatureGeom, vp: Viewport, style: &Style) -> Option<DrawOp> {
    let rings = match &feat.geom {
        GeomKind::Polygon(rs) => project_polygon(rs, vp, true),
        GeomKind::MultiPolygon(parts) => parts.iter().flat_map(|rs| project_polygon(rs, vp, true)).collect(),
        GeomKind::LineString(verts) => vec![project_ring(verts, vp, false)],
        GeomKind::MultiLineString(parts) => parts.iter().map(|p| project_ring(p, vp, false)).collect(),
        // phase 0: points only meaningful with a fill; emit a 1px square. skip
        // if no fill style is configured.
        GeomKind::Point((x, y)) => {
            style.fill?;
            vec![project_point_square(*x, *y, vp)]
        }
        GeomKind::MultiPoint(pts) => {
            style.fill?;
            pts.iter().map(|(x, y)| project_point_square(*x, *y, vp)).collect()
        }
    };
    if rings.is_empty() {
        return None;
    }
    Some(DrawOp::Path {
        path: Path { rings },
        style: style.clone(),
    })
}

fn project_polygon(rings: &[Vec<(f64, f64)>], vp: Viewport, close: bool) -> Vec<Vec<(f32, f32)>> {
    rings.iter().map(|r| project_ring(r, vp, close)).collect()
}

pub(crate) fn project_ring(verts: &[(f64, f64)], vp: Viewport, close: bool) -> Vec<(f32, f32)> {
    let mut out: Vec<(f32, f32)> = verts.iter().map(|&(x, y)| vp.project(x, y)).collect();
    if close && out.len() >= 2 && out[0] != out[out.len() - 1] {
        out.push(out[0]);
    }
    out
}

pub(crate) fn project_point_square(x: f64, y: f64, vp: Viewport) -> Vec<(f32, f32)> {
    let (px, py) = vp.project(x, y);
    vec![(px, py), (px + 1.0, py), (px + 1.0, py + 1.0), (px, py + 1.0), (px, py)]
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
        let out = project_ring(&ring, vp, true);
        assert_eq!(out[0], out[out.len() - 1], "polygon ring should be closed");
    }

    #[test]
    fn project_ring_does_not_close_linestring() {
        let vp = viewport(Bbox::new(0.0, 0.0, 10.0, 10.0), 100, 100);
        let ring = vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)];
        let out = project_ring(&ring, vp, false);
        assert_ne!(out[0], out[out.len() - 1], "linestring should stay open");
    }

    #[test]
    fn project_point_square_1px() {
        let vp = viewport(Bbox::new(0.0, 0.0, 10.0, 10.0), 100, 100);
        let sq = project_point_square(5.0, 5.0, vp);
        assert_eq!(sq.len(), 5);
        assert_eq!(sq[0], sq[4]);
        assert_eq!(sq[1].0, sq[0].0 + 1.0);
    }

    fn build_source(features: &[FeatureGeom]) -> ArtifactReader {
        let mut w = ArtifactWriter::new(ArtifactKind::Source);
        w.add_geometry_payload(features).unwrap();
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
            fill: Some(Colour { r: 255, g: 0, b: 0, a: 255 }),
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
        ss.geometry.insert("red".into(), red_style());
        let mut out = Vec::new();
        emit_layer_cell(
            &src,
            &lyr,
            &ss,
            viewport(Bbox::new(0.0, 0.0, 0.0, 10.0), 100, 100),
            &mut out,
        )
        .unwrap();
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
        ss.geometry.insert("red".into(), red_style());
        let mut out = Vec::new();
        emit_layer_cell(
            &src,
            &lyr,
            &ss,
            viewport(Bbox::new(0.0, 0.0, 10.0, 10.0), 100, 100),
            &mut out,
        )
        .unwrap();
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
        emit_layer_cell(
            &src,
            &lyr,
            &ss,
            viewport(Bbox::new(0.0, 0.0, 10.0, 10.0), 100, 100),
            &mut out,
        )
        .unwrap();
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
        ss.geometry.insert("no_fill".into(), Style::default());
        let mut out = Vec::new();
        emit_layer_cell(
            &src,
            &lyr,
            &ss,
            viewport(Bbox::new(0.0, 0.0, 10.0, 10.0), 100, 100),
            &mut out,
        )
        .unwrap();
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
        ss.geometry.insert("red".into(), red_style());
        let mut out = Vec::new();
        emit_layer_cell(
            &src,
            &lyr,
            &ss,
            viewport(Bbox::new(0.0, 0.0, 10.0, 10.0), 100, 100),
            &mut out,
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        match &out[0] {
            DrawOp::Path { path, .. } => {
                assert_eq!(path.rings.len(), 1);
                assert_eq!(path.rings[0].len(), 5); // 4 corners + close
            }
            other => panic!("expected Path, got {other:?}"),
        }
    }
}

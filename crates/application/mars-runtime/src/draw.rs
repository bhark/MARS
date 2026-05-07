//! per-cell draw-op emission: decode source + layer artifacts, merge-walk by
//! feature_id, viewport bbox filter, world→pixel transform, push DrawOp::Path.

use std::sync::Arc;

use mars_artifact::{
    ArtifactReader, GeomType, GeomVisitor, SectionKind, decode_class_assignment, decode_style_refs, iter_feature_index,
    visit_one_geom,
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
    pub(crate) fn project(self, x: f64, y: f64) -> (f32, f32) {
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

    let class_section = layer.section(SectionKind::ClassAssignment)?;
    let class_assignment = decode_class_assignment(&class_section)?;

    let style_refs_section = layer.section(SectionKind::StyleRefs)?;
    let style_refs = decode_style_refs(&style_refs_section)?;

    // resolve styles by class_index once per layer so the feature loop
    // becomes a Vec index instead of a per-feature BTreeMap-by-String probe.
    // entries are None for style_refs that don't resolve in the stylesheet
    // (the feature path keeps the same skip-and-debug semantics).
    let resolved_styles: Vec<Option<Arc<Style>>> = style_refs
        .iter()
        .map(|name| stylesheet.geometry.get(name).cloned())
        .collect();

    // shared scratch buffers, reused across features. f64_scratch carries
    // ring coords through the reproject FFI; subpaths is the per-feature
    // accumulator drained into a DrawOp.
    let mut f64_scratch: Vec<[f64; 2]> = Vec::new();
    let mut subpaths: Vec<Subpath> = Vec::new();

    // walk the geometry index merge-style against class_assignment (both sorted
    // by id ASC). only decode coords for features that survive both the class
    // join and the canonical-bbox cull. on viewports that intersect ≪ 100% of
    // a cell this skips the bulk of varint decoding.
    let iter = iter_feature_index(&geom_section)?;
    let coord_area = iter.coord_area();
    let mut ai = 0usize;
    for entry in iter {
        let entry = entry?;
        while ai < class_assignment.len() && class_assignment[ai].0 < entry.id {
            ai += 1;
        }
        if ai >= class_assignment.len() || class_assignment[ai].0 != entry.id {
            continue;
        }
        let class_index = class_assignment[ai].1 as usize;
        ai += 1;

        if !bbox_intersects(entry.bbox, canonical_bbox) {
            continue;
        }

        let style = match resolved_styles.get(class_index) {
            Some(Some(s)) => s.clone(),
            Some(None) => {
                tracing::debug!(
                    class_index,
                    style = %style_refs.get(class_index).map(String::as_str).unwrap_or(""),
                    "style missing from stylesheet"
                );
                continue;
            }
            None => {
                tracing::debug!(
                    "class_index {class_index} out of style_refs range ({})",
                    style_refs.len()
                );
                continue;
            }
        };

        let kind = entry.geom_kind()?;
        let close_rings = matches!(kind, GeomType::Polygon | GeomType::MultiPolygon);

        subpaths.clear();
        {
            let mut visitor = RenderVisitor {
                vp: viewport,
                reproject,
                f64_scratch: &mut f64_scratch,
                subpaths: &mut subpaths,
                close_rings,
                style_has_fill: style.fill.is_some(),
                in_ring: false,
                error: None,
            };
            visit_one_geom(coord_area, &entry, &mut visitor)?;
            if let Some(e) = visitor.error.take() {
                return Err(e);
            }
        }
        if subpaths.is_empty() {
            continue;
        }
        out.push(DrawOp::Path {
            path: Path {
                subpaths: std::mem::take(&mut subpaths),
            },
            style,
        });
    }
    Ok(())
}

/// Streaming visitor that converts decoded ring coords into pixel-space
/// `Subpath`s. Per ring it fills `f64_scratch`, runs the optional canonical →
/// request transform once, then projects to f32 pixel space into a fresh
/// `Vec<(f32, f32)>` (the renderer ABI requires owned points). Standalone
/// points (Point / MultiPoint) become 1px squares iff the style has a fill.
struct RenderVisitor<'a, 'b> {
    vp: Viewport,
    reproject: Reproject<'a>,
    /// shared across rings and features: cleared on each `begin_ring`, holds
    /// (x, y) pairs in canonical CRS until `end_ring` reprojects + projects.
    f64_scratch: &'b mut Vec<[f64; 2]>,
    subpaths: &'b mut Vec<Subpath>,
    close_rings: bool,
    style_has_fill: bool,
    in_ring: bool,
    /// Capture reproject errors encountered during traversal — `GeomVisitor`
    /// methods are infallible, so emit_layer_cell drains this after the call.
    error: Option<RuntimeError>,
}

impl GeomVisitor for RenderVisitor<'_, '_> {
    #[inline]
    fn point(&mut self, x: f64, y: f64) {
        if self.error.is_some() {
            return;
        }
        if self.in_ring {
            self.f64_scratch.push([x, y]);
            return;
        }
        // Point / MultiPoint: standalone vertex. emit 1px square if fill set.
        if !self.style_has_fill {
            return;
        }
        let (rx, ry) = match reproject_point(x, y, self.reproject) {
            Ok(v) => v,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        let (px, py) = self.vp.project(rx, ry);
        self.subpaths.push(Subpath {
            points: vec![(px, py), (px + 1.0, py), (px + 1.0, py + 1.0), (px, py + 1.0), (px, py)],
            closed: true,
        });
    }

    fn begin_ring(&mut self) {
        self.f64_scratch.clear();
        self.in_ring = true;
    }

    fn end_ring(&mut self) {
        self.in_ring = false;
        if self.error.is_some() || self.f64_scratch.is_empty() {
            return;
        }
        if let Some(t) = self.reproject
            && let Err(e) = t.transform_points(self.f64_scratch)
        {
            self.error = Some(e.into());
            return;
        }
        let n = self.f64_scratch.len();
        let mut points: Vec<(f32, f32)> = Vec::with_capacity(n + usize::from(self.close_rings));
        for &[x, y] in self.f64_scratch.iter() {
            points.push(self.vp.project(x, y));
        }
        if self.close_rings && points.len() >= 2 && points[0] != points[points.len() - 1] {
            points.push(points[0]);
        }
        self.subpaths.push(Subpath {
            points,
            closed: self.close_rings,
        });
    }

    fn begin_part(&mut self) {}
    fn end_part(&mut self) {}
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
    use std::sync::Arc;

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

    fn build_source(features: Vec<FeatureGeom>) -> ArtifactReader {
        let mut w = ArtifactWriter::new(ArtifactKind::Source);
        let n = features.len() as u64;
        w.add_geometry_payload(features)
            .set_bbox(Bbox::new(0.0, 0.0, 10.0, 10.0))
            .set_feature_count(n);
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
        let src = build_source(vec![FeatureGeom {
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
        let src = build_source(vec![FeatureGeom {
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
        let src = build_source(vec![FeatureGeom {
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
        let src = build_source(vec![FeatureGeom {
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
        let src = build_source(vec![FeatureGeom {
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

    #[test]
    fn polygon_emits_closed_subpath() {
        // GeomKind::Polygon with one open ring (last vertex != first); visitor
        // must close it just like the old project_ring path did.
        let src = build_source(vec![FeatureGeom {
            id: 1,
            bbox: [0.0, 0.0, 10.0, 10.0],
            geom: GeomKind::Polygon(vec![vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)]]),
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
                let pts = &path.subpaths[0].points;
                assert!(path.subpaths[0].closed);
                assert_eq!(pts[0], pts[pts.len() - 1], "polygon ring must close");
            }
            other => panic!("expected Path, got {other:?}"),
        }
    }

    #[test]
    fn linestring_emits_open_subpath() {
        let src = build_source(vec![FeatureGeom {
            id: 1,
            bbox: [0.0, 0.0, 10.0, 10.0],
            geom: GeomKind::LineString(vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)]),
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
                let pts = &path.subpaths[0].points;
                assert!(!path.subpaths[0].closed);
                assert_ne!(pts[0], pts[pts.len() - 1], "linestring must stay open");
            }
            other => panic!("expected Path, got {other:?}"),
        }
    }

    #[test]
    fn multipolygon_emits_one_subpath_per_ring() {
        let src = build_source(vec![FeatureGeom {
            id: 1,
            bbox: [0.0, 0.0, 10.0, 10.0],
            geom: GeomKind::MultiPolygon(vec![
                vec![vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]],
                vec![vec![(2.0, 2.0), (3.0, 2.0), (3.0, 3.0), (2.0, 2.0)]],
            ]),
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
                assert_eq!(path.subpaths.len(), 2);
                assert!(path.subpaths.iter().all(|s| s.closed));
            }
            other => panic!("expected Path, got {other:?}"),
        }
    }
}

//! per-cell draw-op emission: walk decoded source geometry + layer-side
//! class/style refs, merge-walk by feature_id, viewport bbox filter,
//! world→pixel transform, push DrawOp::Path.

use std::sync::Arc;

use mars_artifact::{ArtifactReader, GeomType, SectionKind, decode_class_assignment, decode_style_refs};
use mars_render_port::{DrawOp, Path, Subpath};
use mars_style::{Style, Stylesheet};
use mars_types::{Bbox, CrsCode};

use crate::RuntimeError;
use crate::decoded::DecodedSourceGeometry;

/// optional canonical-to-request reprojection, expressed as a `(from, to)` CRS
/// pair. `Send + Sync` so the emit phase can be parallelised; the actual
/// `Transformer` is resolved per-thread via `mars_proj::cached_transformer`,
/// which keeps the !Send PJ context bound to its construction thread.
pub(crate) type ReprojectPair<'a> = Option<(&'a CrsCode, &'a CrsCode)>;

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

/// emit draw ops for one (decoded source geometry, layer artifact) pair
/// into `out`.
///
/// `canonical_bbox` is the request bbox expressed in the canonical CRS, used
/// to cull feature bboxes (which are stored in canonical coords). `reproject`,
/// when present, names the canonical->request CRS pair; the per-thread
/// transformer cache resolves it to a `Transformer` for the duration of this
/// call. The function is therefore `Send`-safe per invocation, which the
/// rayon emit pass relies on.
pub(crate) fn emit_layer_cell(
    decoded_source: &DecodedSourceGeometry,
    layer: &ArtifactReader,
    stylesheet: &Stylesheet,
    viewport: Viewport,
    canonical_bbox: Bbox,
    reproject: ReprojectPair<'_>,
    out: &mut Vec<DrawOp>,
) -> Result<(), RuntimeError> {
    if viewport.bbox.width() == 0.0 || viewport.bbox.height() == 0.0 {
        // defensive: zero-area viewport produces no draw ops. upstream parsers
        // (wms) already reject this, but non-wms callers may construct a plan
        // directly.
        return Ok(());
    }

    // resolve the per-thread cached transformer once; the Rc keeps it alive
    // for the duration of this call without crossing thread boundaries.
    let transformer = match reproject {
        Some((from, to)) => Some(mars_proj::cached_transformer(from, to)?),
        None => None,
    };
    let reproject = transformer.as_deref();

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

    // scratch reused across features. f64_scratch carries ring coords through
    // the reproject FFI when needed; subpaths is the per-feature accumulator
    // drained into a DrawOp.
    let mut f64_scratch: Vec<[f64; 2]> = Vec::new();
    let mut subpaths: Vec<Subpath> = Vec::new();

    // merge-walk decoded.features against class_assignment (both sorted by id
    // ASC). features are pre-decoded, so the loop is bbox-cull + project +
    // emit; no varint walk per render.
    let mut ai = 0usize;
    for feat in &decoded_source.features {
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

        let standalone = matches!(feat.geom_type, GeomType::Point | GeomType::MultiPoint);
        let close_rings = matches!(feat.geom_type, GeomType::Polygon | GeomType::MultiPolygon);

        subpaths.clear();
        if standalone {
            // Point / MultiPoint: emit one 1px square per vertex iff style has fill.
            if style.fill.is_none() {
                continue;
            }
            for ring in &feat.rings {
                let Some(&[x, y]) = ring.first() else {
                    continue;
                };
                let (rx, ry) = match reproject {
                    Some(t) => t.transform_point(x, y)?,
                    None => (x, y),
                };
                let (px, py) = viewport.project(rx, ry);
                subpaths.push(Subpath {
                    points: vec![(px, py), (px + 1.0, py), (px + 1.0, py + 1.0), (px, py + 1.0), (px, py)],
                    closed: true,
                });
            }
        } else {
            for ring in &feat.rings {
                if ring.is_empty() {
                    continue;
                }
                if let Some(t) = reproject {
                    f64_scratch.clear();
                    f64_scratch.extend_from_slice(ring);
                    t.transform_points(&mut f64_scratch)?;
                    let mut points: Vec<(f32, f32)> = Vec::with_capacity(f64_scratch.len() + usize::from(close_rings));
                    for &[x, y] in f64_scratch.iter() {
                        points.push(viewport.project(x, y));
                    }
                    push_ring_subpath(&mut subpaths, points, close_rings);
                } else {
                    // canonical-CRS == request-CRS: skip the scratch copy and
                    // project the cached coords straight into pixel space.
                    let mut points: Vec<(f32, f32)> = Vec::with_capacity(ring.len() + usize::from(close_rings));
                    for &[x, y] in ring {
                        points.push(viewport.project(x, y));
                    }
                    push_ring_subpath(&mut subpaths, points, close_rings);
                }
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

#[inline]
fn push_ring_subpath(out: &mut Vec<Subpath>, mut points: Vec<(f32, f32)>, close_rings: bool) {
    if close_rings && points.len() >= 2 && points[0] != points[points.len() - 1] {
        points.push(points[0]);
    }
    out.push(Subpath {
        points,
        closed: close_rings,
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use mars_artifact::{ArtifactKind, ArtifactWriter, FeatureGeom, GeomKind};
    use mars_style::{Colour, Style, Stylesheet};
    use mars_types::Bbox;

    use super::*;
    use crate::decoded::decode_source_geometry;

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

    fn build_source(features: Vec<FeatureGeom>) -> DecodedSourceGeometry {
        let mut w = ArtifactWriter::new(ArtifactKind::Source);
        let n = features.len() as u64;
        w.add_geometry_payload(features)
            .set_bbox(Bbox::new(0.0, 0.0, 10.0, 10.0))
            .set_feature_count(n);
        let reader = ArtifactReader::open(w.finish().unwrap()).unwrap();
        decode_source_geometry(&reader).unwrap()
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

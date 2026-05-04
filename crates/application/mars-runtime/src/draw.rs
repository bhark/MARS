//! per-cell draw-op emission: decode source + layer artifacts, merge-walk by
//! feature_id, viewport bbox filter, world→pixel transform, push DrawOp::Path.

use mars_artifact::{
    ArtifactReader, FeatureGeom, GeomKind, SectionKind, decode_class_assignment,
    decode_geometry_payload, decode_style_refs,
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
        let w = self.bbox.width().max(f64::EPSILON);
        let h = self.bbox.height().max(f64::EPSILON);
        let px = (x - self.bbox.min_x) / w * f64::from(self.width);
        // invert Y: world y grows up, pixel y grows down
        let py = (self.bbox.max_y - y) / h * f64::from(self.height);
        (px as f32, py as f32)
    }
}

#[inline]
fn bbox_intersects(feat: [f32; 4], vp: Bbox) -> bool {
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
        GeomKind::MultiPolygon(parts) => parts
            .iter()
            .flat_map(|rs| project_polygon(rs, vp, true))
            .collect(),
        GeomKind::LineString(verts) => vec![project_ring(verts, vp, false)],
        GeomKind::MultiLineString(parts) => {
            parts.iter().map(|p| project_ring(p, vp, false)).collect()
        }
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

fn project_ring(verts: &[(f64, f64)], vp: Viewport, close: bool) -> Vec<(f32, f32)> {
    let mut out: Vec<(f32, f32)> = verts.iter().map(|&(x, y)| vp.project(x, y)).collect();
    if close && out.len() >= 2 && out[0] != out[out.len() - 1] {
        out.push(out[0]);
    }
    out
}

fn project_point_square(x: f64, y: f64, vp: Viewport) -> Vec<(f32, f32)> {
    let (px, py) = vp.project(x, y);
    vec![
        (px, py),
        (px + 1.0, py),
        (px + 1.0, py + 1.0),
        (px, py + 1.0),
        (px, py),
    ]
}

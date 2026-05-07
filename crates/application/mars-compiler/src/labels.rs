//! Per-cell label-candidate emission for the snapshot pipeline. SPEC §14.
//!
//! For each row in the cell, the geometry is decoded once via the WKB reader,
//! the layer's text template is evaluated against the row's attributes, and a
//! [`LabelCandidate`] is produced whose shape mirrors the geometry kind:
//!
//! - point geometries → [`LabelShape::Point`] at the point itself;
//! - polygon geometries → [`LabelShape::PolygonAnchor`] at the outer-ring
//!   bounding-box centre (centroid is reserved for v1.1 — see SPEC §14.1);
//! - line geometries → [`LabelShape::Polyline`] with the original vertex chain.
//!   Arc-length sampling per `Placement::Line { repeat_m, .. }` is left as a
//!   follow-up; v1 emits a single polyline per feature so the runtime renderer
//!   can pick a single placement per feature.
//!
//! Multi-geometries fall back to the first sub-geometry. Foreign-cell
//! replication (SPEC §14.2) is also a follow-up: today every candidate is
//! local, with `foreign_origin = false`.

use mars_artifact::{LabelCandidate, LabelShape};
use mars_expr::eval_template;
use mars_source::{RowAttrs, RowBytes};
use mars_style::LabelStyle;

use crate::CompilerError;
use crate::plan::CompiledLabelSpec;
use crate::wkb;

/// Emit candidates for one cell. `style_ref_idx` must point at the slot the
/// caller appended for the label style in the layer's `style_refs` section.
pub fn emit_candidates(
    rows: &[RowBytes],
    label: &CompiledLabelSpec,
    style_ref_idx: u16,
    label_style: Option<&LabelStyle>,
    expected_srid: Option<u32>,
) -> Result<Vec<LabelCandidate>, CompilerError> {
    let priority: u16 = label_style.map(|s| s.priority).unwrap_or(label.priority);

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let text = eval_template(&label.text, &RowAttrs::new(&row.attributes))?;
        if text.is_empty() {
            continue;
        }
        let Some(shape) = candidate_shape(&row.geometry, expected_srid)? else {
            continue;
        };
        out.push(LabelCandidate {
            feature_id: row.feature_id,
            foreign_origin: false,
            priority,
            style_ref_idx,
            shape,
            text,
        });
    }
    out.sort_by_key(|c| c.feature_id);
    Ok(out)
}

fn candidate_shape(wkb_bytes: &[u8], expected_srid: Option<u32>) -> Result<Option<LabelShape>, CompilerError> {
    use mars_artifact::GeomKind;

    // a label candidate uses the synthetic feature_id `0`; only the geometry
    // shape is read out below, so the id is irrelevant.
    let feature = wkb::decode_feature(0, wkb_bytes, expected_srid)?;
    Ok(match feature.geom {
        GeomKind::Point(p) => Some(LabelShape::Point {
            x: p.0 as f32,
            y: p.1 as f32,
        }),
        GeomKind::LineString(verts) if !verts.is_empty() => Some(LabelShape::Polyline(
            verts.into_iter().map(|(x, y)| (x as f32, y as f32)).collect(),
        )),
        GeomKind::Polygon(rings) => rings
            .first()
            .filter(|r| !r.is_empty())
            .map(|ring| anchor_from_ring(ring))
            .map(|(x, y)| LabelShape::PolygonAnchor { x, y }),
        GeomKind::MultiPoint(pts) => pts.first().map(|p| LabelShape::Point {
            x: p.0 as f32,
            y: p.1 as f32,
        }),
        GeomKind::MultiLineString(lines) => lines
            .into_iter()
            .find(|l| !l.is_empty())
            .map(|verts| LabelShape::Polyline(verts.into_iter().map(|(x, y)| (x as f32, y as f32)).collect())),
        GeomKind::MultiPolygon(polys) => polys
            .into_iter()
            .next()
            .and_then(|rings| rings.into_iter().next())
            .filter(|r| !r.is_empty())
            .map(|ring| {
                let (x, y) = anchor_from_ring(&ring);
                LabelShape::PolygonAnchor { x, y }
            }),
        GeomKind::LineString(_) => None,
    })
}

/// Bounding-box centre of a ring. Stand-in for true centroid until the v1.1
/// upgrade brings in `geo::Polygon::centroid` (SPEC §14.1).
fn anchor_from_ring(ring: &[(f64, f64)]) -> (f32, f32) {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for &(x, y) in ring {
        if x < min_x {
            min_x = x;
        }
        if x > max_x {
            max_x = x;
        }
        if y < min_y {
            min_y = y;
        }
        if y > max_y {
            max_y = y;
        }
    }
    (((min_x + max_x) * 0.5) as f32, ((min_y + max_y) * 0.5) as f32)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use mars_expr::parse_template;
    use mars_style::Placement;

    fn point_wkb(x: f64, y: f64) -> Bytes {
        // little-endian WKB point: 0x01, type=1, x, y
        let mut v = Vec::with_capacity(21);
        v.push(1u8);
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
        Bytes::from(v)
    }

    fn label_spec(text: &str) -> CompiledLabelSpec {
        CompiledLabelSpec {
            text: parse_template(text).unwrap(),
            placement: Placement::Point,
            style_id: "lbl".into(),
            priority: 7,
        }
    }

    fn row(id: u64, x: f64, y: f64, attrs: Vec<(String, mars_source::AttrValue)>) -> RowBytes {
        RowBytes {
            feature_id: id,
            geometry: point_wkb(x, y),
            attributes: attrs,
        }
    }

    #[test]
    fn emits_one_candidate_per_row() {
        let label = label_spec("{name}");
        let rows = vec![
            row(
                1,
                10.0,
                20.0,
                vec![("name".into(), mars_source::AttrValue::String("a".into()))],
            ),
            row(
                2,
                30.0,
                40.0,
                vec![("name".into(), mars_source::AttrValue::String("b".into()))],
            ),
        ];
        let cs = emit_candidates(&rows, &label, 5, None, None).unwrap();
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].text, "a");
        assert_eq!(cs[0].priority, 7);
        assert_eq!(cs[0].style_ref_idx, 5);
        assert!(
            matches!(cs[0].shape, LabelShape::Point { x, y } if (x - 10.0).abs() < 1e-3 && (y - 20.0).abs() < 1e-3)
        );
        assert!(!cs[0].foreign_origin);
    }

    #[test]
    fn skips_rows_with_empty_text() {
        let label = label_spec("{name}");
        let rows = vec![
            row(1, 10.0, 20.0, vec![]),
            row(
                2,
                30.0,
                40.0,
                vec![("name".into(), mars_source::AttrValue::String("x".into()))],
            ),
        ];
        let cs = emit_candidates(&rows, &label, 0, None, None).unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].feature_id, 2);
    }
}

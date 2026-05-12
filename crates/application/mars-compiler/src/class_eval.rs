//! per-feature class assignment + label-candidate emission.
//!
//! both bootstrap (snapshot.rs) and incremental rebuild paths invoke these
//! after each page is built: walk the page's features through the layer's
//! pre-parsed `when:` expressions and label spec, produce
//!   - `Vec<(feature_id, class_index)>` for the class-assignment sidecar, and
//!   - `Vec<LabelCandidate>` for the label sidecar.
//!
//! evaluation runs in-process via `mars_expr::eval`. SQL-side lowering is
//! preserved in `mars-source-postgres::lower` for runtime read paths but not
//! used at compile time so the snapshot and rebuild paths share one code
//! path against fetched rows.

use mars_artifact::{FeatureGeom, GeomKind, LabelCandidate, LabelShape};
use mars_expr::{AttributeAccess, Expr, Literal, Segment, Template, eval};
use mars_source::AttrValue;
use mars_style::{LabelSurvival, Placement, PolygonStrategy};

use crate::polylabel;

/// adapter over a row's attribute slice. `mars-source::AttrValue` maps 1:1 to
/// `mars-expr::Literal`; missing names return `None` (becomes
/// `ExprError::UnknownIdent` only if the expression actually reads the name).
pub struct RowAttrs<'a> {
    pub fields: &'a [(String, AttrValue)],
}

impl<'a> RowAttrs<'a> {
    #[must_use]
    pub fn new(fields: &'a [(String, AttrValue)]) -> Self {
        Self { fields }
    }
}

impl<'a> AttributeAccess for RowAttrs<'a> {
    fn get(&self, name: &str) -> Option<Literal> {
        self.fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| attr_to_literal(v.clone()))
    }
}

fn attr_to_literal(v: AttrValue) -> Literal {
    match v {
        AttrValue::Null => Literal::Null,
        AttrValue::Bool(b) => Literal::Bool(b),
        AttrValue::Int(i) => Literal::Int(i),
        AttrValue::Float(f) => Literal::Float(f),
        AttrValue::String(s) => Literal::String(s),
    }
}

/// walk classes top-down, first match wins. an entry with `None` is a
/// catch-all. returns the class index, or `None` when no class matches
/// (the renderer skips the feature). class index is bounded by `u16` so
/// it fits the wire format of the class-assignment sidecar; we surface
/// `None` for layers with `> u16::MAX` classes rather than truncate.
#[must_use]
pub fn assign_class<A: AttributeAccess>(when_clauses: &[Option<Expr>], attrs: &A) -> Option<u16> {
    for (idx, when) in when_clauses.iter().enumerate() {
        let class_index: u16 = match u16::try_from(idx) {
            Ok(v) => v,
            Err(_) => return None,
        };
        let matches = match when {
            None => true,
            Some(expr) => matches!(eval(expr, attrs), Ok(Literal::Bool(true))),
        };
        if matches {
            return Some(class_index);
        }
    }
    None
}

/// resolved label spec carried from the layer plan into the per-feature
/// emit path. `text` is pre-parsed; `placement` is fully resolved to one
/// of the three placement variants.
#[derive(Debug, Clone)]
pub struct LabelSpec<'a> {
    pub priority: u16,
    pub text: &'a Template,
    pub placement: &'a Placement,
    pub style_ref_idx: u16,
}

/// decide whether this feature emits a label candidate at this level, and
/// build it. `feature_idx` is the per-page slot when the feature was paged
/// at this level, or `None` when its geometry was pruned by the level's
/// `passes_min_size` filter.
///
/// Decimation: with `Independent` the label survives even when
/// the geometry is pruned (prevents floating-anchor-with-no-feature at
/// low zoom); with `FollowGeometry` the label is dropped alongside.
#[must_use]
pub fn emit_label_candidate<A: AttributeAccess>(
    feature: &FeatureGeom,
    feature_idx: Option<u32>,
    attrs: &A,
    spec: &LabelSpec<'_>,
    survival: LabelSurvival,
    min_priority: u32,
) -> Option<LabelCandidate> {
    if feature_idx.is_none() && matches!(survival, LabelSurvival::FollowGeometry) {
        return None;
    }
    if u32::from(spec.priority) < min_priority {
        return None;
    }
    let text = expand_template(spec.text, attrs).ok()?;
    let shape = label_shape_from_geom(feature, spec.placement)?;
    Some(LabelCandidate {
        feature_idx,
        foreign_origin: false,
        priority: spec.priority,
        style_ref_idx: spec.style_ref_idx,
        shape,
        text,
    })
}

fn expand_template<A: AttributeAccess>(t: &Template, attrs: &A) -> Result<String, ()> {
    // mars_expr::eval_template returns ExprError on missing idents; we treat
    // any failure as "drop this candidate" rather than abort the page.
    let mut out = String::new();
    for seg in &t.segments {
        match seg {
            Segment::Literal(s) => out.push_str(s),
            Segment::Ident(name) => match attrs.get(name) {
                Some(Literal::Null) | None => return Err(()),
                Some(Literal::Bool(b)) => out.push_str(if b { "true" } else { "false" }),
                Some(Literal::Int(i)) => out.push_str(&i.to_string()),
                Some(Literal::Float(f)) => out.push_str(&f.to_string()),
                Some(Literal::String(s)) => out.push_str(&s),
            },
        }
    }
    Ok(out)
}

fn label_shape_from_geom(feature: &FeatureGeom, placement: &Placement) -> Option<LabelShape> {
    match (&feature.geom, placement) {
        (GeomKind::Point((x, y)), _) => Some(LabelShape::Point {
            x: *x as f32,
            y: *y as f32,
        }),
        (GeomKind::MultiPoint(pts), _) => pts.first().map(|&(x, y)| LabelShape::Point {
            x: x as f32,
            y: y as f32,
        }),
        (GeomKind::LineString(line), Placement::Line { .. }) if !line.is_empty() => Some(LabelShape::Polyline(
            line.iter().map(|&(x, y)| (x as f32, y as f32)).collect(),
        )),
        (GeomKind::MultiLineString(parts), Placement::Line { .. }) => parts
            .iter()
            .find(|p| !p.is_empty())
            .map(|p| LabelShape::Polyline(p.iter().map(|&(x, y)| (x as f32, y as f32)).collect())),
        (GeomKind::LineString(line), _) if !line.is_empty() => {
            let mid = line[line.len() / 2];
            Some(LabelShape::Point {
                x: mid.0 as f32,
                y: mid.1 as f32,
            })
        }
        (g @ (GeomKind::Polygon(_) | GeomKind::MultiPolygon(_)), Placement::Polygon { strategy }) => {
            let poly = polylabel::pick_largest_polygon(g)?;
            let (x, y) = match strategy {
                PolygonStrategy::Polylabel => {
                    let prec = polylabel::default_precision(poly);
                    polylabel::pole_of_inaccessibility(poly, prec)
                }
                PolygonStrategy::Centroid => polylabel::centroid(poly),
            };
            Some(LabelShape::PolygonAnchor {
                x: x as f32,
                y: y as f32,
            })
        }
        // placement / geometry mismatch: drop. catches e.g. polygon placement
        // applied to a line-typed layer; config validation rejects most cases
        // already.
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_expr::{parse, parse_template};

    fn feature(user_id: u64, geom: GeomKind, bbox: [f32; 4]) -> FeatureGeom {
        FeatureGeom { user_id, bbox, geom }
    }

    #[test]
    fn first_match_wins() {
        let when0 = parse("kind = 'major'").unwrap();
        let when1 = parse("kind = 'minor'").unwrap();
        let clauses = vec![Some(when0), Some(when1), None];

        let row = vec![("kind".to_string(), AttrValue::String("minor".into()))];
        let attrs = RowAttrs::new(&row);
        assert_eq!(assign_class(&clauses, &attrs), Some(1));

        let row = vec![("kind".to_string(), AttrValue::String("path".into()))];
        let attrs = RowAttrs::new(&row);
        // catch-all wins
        assert_eq!(assign_class(&clauses, &attrs), Some(2));
    }

    #[test]
    fn assign_class_returns_none_when_no_match_and_no_catch_all() {
        let when0 = parse("kind = 'major'").unwrap();
        let clauses = vec![Some(when0)];
        let row = vec![("kind".to_string(), AttrValue::String("minor".into()))];
        let attrs = RowAttrs::new(&row);
        assert_eq!(assign_class(&clauses, &attrs), None);
    }

    #[test]
    fn label_kept_for_pruned_geom_under_independent() {
        let f = feature(7, GeomKind::Point((1.0, 2.0)), [1.0, 2.0, 1.0, 2.0]);
        let row = vec![("name".into(), AttrValue::String("A".into()))];
        let attrs = RowAttrs::new(&row);
        let template = parse_template("{name}").unwrap();
        let placement = Placement::Point;
        let spec = LabelSpec {
            priority: 100,
            text: &template,
            placement: &placement,
            style_ref_idx: 0,
        };
        let cand = emit_label_candidate(&f, None, &attrs, &spec, LabelSurvival::Independent, 0).unwrap();
        assert_eq!(cand.feature_idx, None);
        assert_eq!(cand.text, "A");
    }

    #[test]
    fn label_dropped_for_pruned_geom_under_follow_geometry() {
        let f = feature(7, GeomKind::Point((1.0, 2.0)), [1.0, 2.0, 1.0, 2.0]);
        let row = vec![("name".into(), AttrValue::String("A".into()))];
        let attrs = RowAttrs::new(&row);
        let template = parse_template("{name}").unwrap();
        let placement = Placement::Point;
        let spec = LabelSpec {
            priority: 100,
            text: &template,
            placement: &placement,
            style_ref_idx: 0,
        };
        let cand = emit_label_candidate(&f, None, &attrs, &spec, LabelSurvival::FollowGeometry, 0);
        assert!(cand.is_none());
    }

    #[test]
    fn label_dropped_below_min_priority() {
        let f = feature(7, GeomKind::Point((1.0, 2.0)), [1.0, 2.0, 1.0, 2.0]);
        let row = vec![("name".into(), AttrValue::String("A".into()))];
        let attrs = RowAttrs::new(&row);
        let template = parse_template("{name}").unwrap();
        let placement = Placement::Point;
        let spec = LabelSpec {
            priority: 5,
            text: &template,
            placement: &placement,
            style_ref_idx: 0,
        };
        assert!(emit_label_candidate(&f, Some(0), &attrs, &spec, LabelSurvival::Independent, 10).is_none());
    }

    #[test]
    fn polygon_anchor_uses_bbox_centroid() {
        let f = feature(
            42,
            GeomKind::Polygon(vec![vec![
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
                (0.0, 0.0),
            ]]),
            [0.0, 0.0, 10.0, 10.0],
        );
        let row = vec![("name".into(), AttrValue::String("Sq".into()))];
        let attrs = RowAttrs::new(&row);
        let template = parse_template("{name}").unwrap();
        let placement = Placement::Polygon {
            strategy: PolygonStrategy::Centroid,
        };
        let spec = LabelSpec {
            priority: 0,
            text: &template,
            placement: &placement,
            style_ref_idx: 0,
        };
        let cand = emit_label_candidate(&f, Some(0), &attrs, &spec, LabelSurvival::Independent, 0).unwrap();
        match cand.shape {
            LabelShape::PolygonAnchor { x, y } => {
                assert!((x - 5.0).abs() < f32::EPSILON);
                assert!((y - 5.0).abs() < f32::EPSILON);
            }
            _ => panic!("expected polygon anchor"),
        }
    }

    #[test]
    fn line_with_line_placement_emits_polyline() {
        let coords = vec![(0.0, 0.0), (1.0, 0.0), (2.0, 0.0)];
        let f = feature(1, GeomKind::LineString(coords.clone()), [0.0, 0.0, 2.0, 0.0]);
        let row = vec![("name".into(), AttrValue::String("L".into()))];
        let attrs = RowAttrs::new(&row);
        let template = parse_template("{name}").unwrap();
        let placement = Placement::Line {
            repeat_m: 250.0,
            max_angle_delta_deg: 25.0,
        };
        let spec = LabelSpec {
            priority: 0,
            text: &template,
            placement: &placement,
            style_ref_idx: 0,
        };
        let cand = emit_label_candidate(&f, Some(0), &attrs, &spec, LabelSurvival::Independent, 0).unwrap();
        match cand.shape {
            LabelShape::Polyline(pts) => assert_eq!(pts.len(), 3),
            _ => panic!("expected polyline"),
        }
    }
}

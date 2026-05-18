#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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
        angle_mode: mars_style::LineAngleMode::Auto,
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

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn placement_round_trips_each_variant() {
    let p: Placement = serde_yaml_ng::from_str("kind: point").unwrap();
    assert!(matches!(p, Placement::Point));

    let p: Placement = serde_yaml_ng::from_str("kind: line").unwrap();
    match p {
        Placement::Line {
            repeat_m,
            max_angle_delta_deg,
            angle_mode,
        } => {
            assert!((repeat_m - 250.0).abs() < f64::EPSILON);
            assert!((max_angle_delta_deg - 25.0).abs() < f32::EPSILON);
            assert_eq!(angle_mode, LineAngleMode::Auto);
        }
        _ => panic!("expected line"),
    }

    let p: Placement = serde_yaml_ng::from_str("kind: line\nrepeat_m: 100\nmax_angle_delta_deg: 10").unwrap();
    match p {
        Placement::Line {
            repeat_m,
            max_angle_delta_deg,
            angle_mode,
        } => {
            assert!((repeat_m - 100.0).abs() < f64::EPSILON);
            assert!((max_angle_delta_deg - 10.0).abs() < f32::EPSILON);
            assert_eq!(angle_mode, LineAngleMode::Auto);
        }
        _ => panic!("expected line"),
    }

    let p: Placement = serde_yaml_ng::from_str("kind: polygon").unwrap();
    assert!(matches!(
        p,
        Placement::Polygon {
            strategy: PolygonStrategy::Polylabel
        }
    ));

    let p: Placement = serde_yaml_ng::from_str("kind: polygon\nstrategy: polylabel").unwrap();
    assert!(matches!(
        p,
        Placement::Polygon {
            strategy: PolygonStrategy::Polylabel
        }
    ));

    let p: Placement = serde_yaml_ng::from_str("kind: polygon\nstrategy: centroid").unwrap();
    assert!(matches!(
        p,
        Placement::Polygon {
            strategy: PolygonStrategy::Centroid
        }
    ));

    // one-release migration alias: legacy `inner_skeleton` must parse and
    // map to Polylabel.
    let p: Placement = serde_yaml_ng::from_str("kind: polygon\nstrategy: inner_skeleton").unwrap();
    assert!(matches!(
        p,
        Placement::Polygon {
            strategy: PolygonStrategy::Polylabel
        }
    ));
}

#[test]
fn label_survival_round_trips_and_defaults_independent() {
    // default
    assert!(matches!(LabelSurvival::default(), LabelSurvival::Independent));
    // wire form is snake_case
    let i: LabelSurvival = serde_yaml_ng::from_str("independent").unwrap();
    assert!(matches!(i, LabelSurvival::Independent));
    let f: LabelSurvival = serde_yaml_ng::from_str("follow_geometry").unwrap();
    assert!(matches!(f, LabelSurvival::FollowGeometry));
}

#[test]
fn label_style_from_spec_example_round_trips() {
    let json = r##"{
            "font_family": "Arial",
            "font_size": 12,
            "fill": "#000000",
            "halo": { "color": "#ffffff", "width": 1.5 },
            "priority": 100,
            "min_distance": 50
        }"##;
    let l: LabelStyle = serde_json::from_str(json).unwrap();
    assert_eq!(l.font_family, "Arial");
    assert_eq!(l.font_size, ScaledSize::from_px(12.0));
    assert_eq!(l.fill, Colour::rgba(0, 0, 0, 0xff));
    let halo = l.halo.unwrap();
    assert_eq!(halo.colour, Colour::rgba(0xff, 0xff, 0xff, 0xff));
    assert!((halo.width - 1.5).abs() < f32::EPSILON);
    // new fields default to the back-compat values so existing configs
    // keep their current behaviour.
    assert_eq!(l.position, AnchorPosition::Auto);
    assert_eq!(l.offset_px, (0.0, 0.0));
    assert!(l.angle.is_none());
    assert!(!l.partials);
    assert!(!l.force);
}

#[test]
fn label_style_round_trips_new_fields() {
    let yaml = r#"
font_family: Arial
font_size: 12
fill: '#000000'
position: uc
offset_px: [3.5, -2.0]
angle_deg: 45.0
partials: true
force: true
min_distance: 8.0
"#;
    let l: LabelStyle = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(l.position, AnchorPosition::Uc);
    assert_eq!(l.offset_px, (3.5, -2.0));
    assert_eq!(l.angle, Some(NumericField::Static(45.0)));
    assert!(l.partials);
    assert!(l.force);
    assert!((l.min_distance - 8.0).abs() < f32::EPSILON);

    // serialise back and reparse: round-trip must preserve the new fields.
    let out = serde_yaml_ng::to_string(&l).unwrap();
    let back: LabelStyle = serde_yaml_ng::from_str(&out).unwrap();
    assert_eq!(back, l);
}

#[test]
fn anchor_position_wire_form_is_short_lowercase() {
    for (pos, wire) in [
        (AnchorPosition::Ul, "ul"),
        (AnchorPosition::Uc, "uc"),
        (AnchorPosition::Ur, "ur"),
        (AnchorPosition::Cl, "cl"),
        (AnchorPosition::Cc, "cc"),
        (AnchorPosition::Cr, "cr"),
        (AnchorPosition::Ll, "ll"),
        (AnchorPosition::Lc, "lc"),
        (AnchorPosition::Lr, "lr"),
        (AnchorPosition::Auto, "auto"),
    ] {
        let out = serde_yaml_ng::to_string(&pos).unwrap();
        assert_eq!(out.trim(), wire);
        let back: AnchorPosition = serde_yaml_ng::from_str(wire).unwrap();
        assert_eq!(back, pos);
    }
}

#[test]
fn line_angle_mode_round_trips() {
    let p: Placement = serde_yaml_ng::from_str("kind: line\nangle_mode: follow").unwrap();
    match p {
        Placement::Line { angle_mode, .. } => assert_eq!(angle_mode, LineAngleMode::Follow),
        _ => panic!("expected line"),
    }
    let p: Placement = serde_yaml_ng::from_str("kind: line\nangle_mode: auto").unwrap();
    match p {
        Placement::Line { angle_mode, .. } => assert_eq!(angle_mode, LineAngleMode::Auto),
        _ => panic!("expected line"),
    }
}

#[test]
fn label_style_resolve_collapses_font_size() {
    let l = LabelStyle {
        font_family: "Sans".into(),
        font_size: ScaledSize {
            base_px: 12.0,
            min_px: Some(6.0),
            max_px: Some(24.0),
            ref_denom: Some(50_000),
            attribute: None,
        },
        fill: Colour::rgba(0, 0, 0, 0xff),
        halo: None,
        priority: 100,
        min_distance: 0.0,
        position: AnchorPosition::Auto,
        offset_px: (0.0, 0.0),
        angle: None,
        partials: false,
        force: false,
    };
    let r = l.resolve(25_000);
    assert!((r.font_size - 24.0).abs() < f32::EPSILON);
    assert_eq!(r.font_family, "Sans");
    assert_eq!(r.priority, 100);
}

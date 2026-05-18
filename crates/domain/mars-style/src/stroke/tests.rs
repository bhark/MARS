#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn blend_mode_serializes_kebab_case() {
    let yaml = serde_yaml_ng::to_string(&BlendMode::SourceOver).unwrap();
    assert!(yaml.trim() == "source-over");
    let parsed: BlendMode = serde_yaml_ng::from_str("source-over").unwrap();
    assert_eq!(parsed, BlendMode::SourceOver);
}

#[test]
fn stroke_gap_initial_defaults_to_zero() {
    let g: StrokeGap = serde_yaml_ng::from_str("interval_px: 8.0\n").unwrap();
    assert!((g.interval_px - 8.0).abs() < f32::EPSILON);
    assert!(g.initial_px.abs() < f32::EPSILON);
}

#[test]
fn geom_transform_wire_form_is_snake_case() {
    for (variant, wire) in [
        (GeomTransform::Start, "start"),
        (GeomTransform::End, "end"),
        (GeomTransform::Vertices, "vertices"),
    ] {
        let out = serde_yaml_ng::to_string(&variant).unwrap();
        assert_eq!(out.trim(), wire);
        let back: GeomTransform = serde_yaml_ng::from_str(wire).unwrap();
        assert_eq!(back, variant);
    }
}

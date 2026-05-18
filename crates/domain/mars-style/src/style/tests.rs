#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn default_style_is_empty() {
    let s = Style::default();
    assert!(s.fill.is_none());
    assert!(s.stroke.is_none());
}

#[test]
fn polygon_style_from_spec_example_round_trips() {
    // bare-hex fill must deserialise as Solid (wire-format symmetry).
    let yaml = "fill: '#fafafa'\nstroke: '#b4b4b4'\nstroke_width: 0.6\n";
    let s: Style = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(matches!(s.fill, Some(FillPaint::Solid(c)) if c == Colour::rgba(0xfa, 0xfa, 0xfa, 0xff)));
    assert_eq!(s.stroke.unwrap(), Colour::rgba(0xb4, 0xb4, 0xb4, 0xff));
    // bare f32 wire form lands in ScaledSize::from_px.
    assert_eq!(s.stroke_width.unwrap(), ScaledSize::from_px(0.6));
}

#[test]
fn style_with_marker_roundtrip() {
    let yaml = "stroke: '#000000'\nstroke_width: 1.0\nfill: '#ff0000'\nmarker:\n  kind: pin\n  size: 10.0\n";
    let s: Style = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(matches!(s.fill, Some(FillPaint::Solid(c)) if c == Colour::rgba(0xff, 0, 0, 0xff)));
    let m = s.marker.expect("marker present");
    assert_eq!(m.shape, MarkerShape::Pin);
    assert!((m.size.base_px - 10.0).abs() < f32::EPSILON);
}

#[test]
fn style_opacity_offset_gap_default_to_none() {
    let s = Style::default();
    assert!(s.opacity.is_none());
    assert!(s.stroke_offset_px.is_none());
    assert!(s.stroke_gap.is_none());
}

#[test]
fn style_opacity_offset_gap_roundtrip_yaml() {
    let yaml = "stroke: '#000000'\nstroke_width: 1.0\nopacity: 0.5\nstroke_offset_px: 2.0\nstroke_gap:\n  interval_px: 12.0\n  initial_px: 3.0\n";
    let s: Style = serde_yaml_ng::from_str(yaml).unwrap();
    assert!((s.opacity.unwrap() - 0.5).abs() < f32::EPSILON);
    assert!((s.stroke_offset_px.unwrap() - 2.0).abs() < f32::EPSILON);
    let g = s.stroke_gap.unwrap();
    assert!((g.interval_px - 12.0).abs() < f32::EPSILON);
    assert!((g.initial_px - 3.0).abs() < f32::EPSILON);
}

#[test]
fn style_blend_mode_defaults_to_none_and_round_trips() {
    let s = Style::default();
    assert!(s.blend_mode.is_none());

    let yaml = "blend_mode: multiply\n";
    let s: Style = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(s.blend_mode, Some(BlendMode::Multiply));

    // resolve passes blend_mode through unchanged.
    let r = s.resolve(1000);
    assert_eq!(r.blend_mode, Some(BlendMode::Multiply));
}

#[test]
fn style_geom_transform_defaults_to_none() {
    let s: Style = serde_yaml_ng::from_str("stroke: '#000000'\n").unwrap();
    assert!(s.geom_transform.is_none());
}

#[test]
fn style_with_geom_transform_round_trips() {
    let yaml = "stroke: '#000000'\ngeom_transform: vertices\n";
    let s: Style = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(s.geom_transform, Some(GeomTransform::Vertices));
    let out = serde_yaml_ng::to_string(&s).unwrap();
    assert!(out.contains("geom_transform: vertices"));
}

#[test]
fn style_resolve_collapses_stroke_width_against_denom() {
    let s = Style {
        stroke: Some(Colour::rgba(0, 0, 0, 0xff)),
        stroke_width: Some(ScaledSize {
            base_px: 10.0,
            min_px: Some(2.0),
            max_px: Some(20.0),
            ref_denom: Some(50_000),
            attribute: None,
        }),
        ..Default::default()
    };
    // at half the ref denom: 2x scaling, clamped at max_px=20.
    let r = s.resolve(25_000);
    assert!((r.stroke_width.unwrap() - 20.0).abs() < f32::EPSILON);
    // at 2x: half size, no clamp (5.0).
    let r = s.resolve(100_000);
    assert!((r.stroke_width.unwrap() - 5.0).abs() < f32::EPSILON);
    // far out: clamped to min_px=2.
    let r = s.resolve(2_000_000);
    assert!((r.stroke_width.unwrap() - 2.0).abs() < f32::EPSILON);
}

#[test]
fn style_resolve_passes_through_non_size_fields_unchanged() {
    let s = Style {
        fill: Some(FillPaint::Solid(Colour::rgba(0xff, 0, 0, 0xff))),
        stroke: Some(Colour::rgba(0, 0xff, 0, 0xff)),
        stroke_width: Some(ScaledSize::from_px(1.5)),
        stroke_dasharray: Some(vec![4.0, 2.0]),
        opacity: Some(0.5),
        ..Default::default()
    };
    let r = s.resolve(50_000);
    assert!(matches!(r.fill, Some(FillPaint::Solid(c)) if c == Colour::rgba(0xff, 0, 0, 0xff)));
    assert_eq!(r.stroke.unwrap(), Colour::rgba(0, 0xff, 0, 0xff));
    assert_eq!(r.stroke_dasharray.as_deref(), Some(&[4.0, 2.0][..]));
    assert!((r.opacity.unwrap() - 0.5).abs() < f32::EPSILON);
    assert!((r.stroke_width.unwrap() - 1.5).abs() < f32::EPSILON);
}

#[test]
fn style_resolve_marker_size_collapses() {
    let s = Style {
        fill: Some(FillPaint::Solid(Colour::rgba(0, 0, 0, 0xff))),
        marker: Some(MarkerSymbol {
            shape: MarkerShape::Circle,
            size: ScaledSize {
                base_px: 8.0,
                min_px: None,
                max_px: None,
                ref_denom: Some(50_000),
                attribute: None,
            },
            angle: None,
        }),
        ..Default::default()
    };
    let r = s.resolve(25_000);
    let m = r.marker.expect("marker resolved");
    assert!((m.size - 16.0).abs() < f32::EPSILON);
    assert_eq!(m.shape, MarkerShape::Circle);
}

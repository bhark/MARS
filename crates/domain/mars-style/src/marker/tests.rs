#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn marker_symbol_yaml_roundtrip() {
    let yaml = "kind: circle\nsize: 8.0\n";
    let m: MarkerSymbol = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(m.shape, MarkerShape::Circle);
    assert!((m.size.base_px - 8.0).abs() < f32::EPSILON);
    let out = serde_yaml_ng::to_string(&m).unwrap();
    assert!(out.contains("kind: circle"));
    assert!(out.contains("size: 8"));
}

#[test]
fn marker_symbol_default_size_kicks_in() {
    let m: MarkerSymbol = serde_yaml_ng::from_str("kind: triangle").unwrap();
    assert_eq!(m.shape, MarkerShape::Triangle);
    assert!((m.size.base_px - 6.0).abs() < f32::EPSILON);
}

#[test]
fn marker_symbol_default_size_for_glyph_is_twelve() {
    let yaml = "kind: glyph\nfont_family: Sans\nch: A\n";
    let m: MarkerSymbol = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(matches!(m.shape, MarkerShape::Glyph { .. }));
    assert!((m.size.base_px - 12.0).abs() < f32::EPSILON);
}

#[test]
fn marker_vector_shape_roundtrip() {
    let yaml = "kind: vector_shape\npoints: [[0.0, 0.0], [1.0, 0.0], [0.5, 1.0]]\nsize: 10.0\n";
    let m: MarkerSymbol = serde_yaml_ng::from_str(yaml).unwrap();
    match m.shape {
        MarkerShape::VectorShape { points, anchor, filled } => {
            assert_eq!(points.len(), 3);
            assert!((anchor.0 - 0.5).abs() < f32::EPSILON);
            assert!((anchor.1 - 0.5).abs() < f32::EPSILON);
            assert!(filled);
        }
        _ => panic!("expected vector_shape"),
    }
    assert!((m.size.base_px - 10.0).abs() < f32::EPSILON);
}

#[test]
fn marker_glyph_roundtrip_accepts_character_alias() {
    let yaml = "kind: glyph\nfont_family: \"Sans\"\ncharacter: \"T\"\nsize: 14.0\n";
    let m: MarkerSymbol = serde_yaml_ng::from_str(yaml).unwrap();
    match m.shape {
        MarkerShape::Glyph { font_family, ch } => {
            assert_eq!(font_family, "Sans");
            assert_eq!(ch, "T");
        }
        _ => panic!("expected glyph"),
    }
    assert!((m.size.base_px - 14.0).abs() < f32::EPSILON);
}

#[test]
fn marker_base_size_works_for_all_variants() {
    assert!(
        (MarkerSymbol {
            shape: MarkerShape::Circle,
            size: ScaledSize::from_px(7.0),
            angle: None,
        }
        .base_size()
            - 7.0)
            .abs()
            < f32::EPSILON
    );
    assert!(
        (MarkerSymbol {
            shape: MarkerShape::VectorShape {
                points: vec![(0.0, 0.0), (1.0, 0.0), (0.5, 1.0)],
                anchor: (0.5, 0.5),
                filled: true,
            },
            size: ScaledSize::from_px(9.0),
            angle: None,
        }
        .base_size()
            - 9.0)
            .abs()
            < f32::EPSILON
    );
    assert!(
        (MarkerSymbol {
            shape: MarkerShape::Glyph {
                font_family: "Sans".into(),
                ch: "X".into(),
            },
            size: ScaledSize::from_px(11.0),
            angle: None,
        }
        .base_size()
            - 11.0)
            .abs()
            < f32::EPSILON
    );
}

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn colour_parses_rrggbb() {
    let c: Colour = "#fafafa".parse().unwrap();
    assert_eq!(c, Colour::rgba(0xfa, 0xfa, 0xfa, 0xff));
}

#[test]
fn colour_parses_rrggbbaa() {
    let c: Colour = "#01020380".parse().unwrap();
    assert_eq!(c, Colour::rgba(1, 2, 3, 0x80));
}

#[test]
fn colour_rejects_garbage() {
    assert!("fafafa".parse::<Colour>().is_err());
    assert!("#fafaf".parse::<Colour>().is_err());
    assert!("#zzzzzz".parse::<Colour>().is_err());
}

#[test]
fn colour_round_trip_serde() {
    let c = Colour::rgba(0xfa, 0xfa, 0xfa, 0xff);
    let json = serde_json::to_string(&c).unwrap();
    assert_eq!(json, "\"#fafafa\"");
    let back: Colour = serde_json::from_str(&json).unwrap();
    assert_eq!(c, back);
}

#[test]
fn colour_round_trip_with_alpha() {
    let c = Colour::rgba(1, 2, 3, 0x80);
    let json = serde_json::to_string(&c).unwrap();
    assert_eq!(json, "\"#01020380\"");
    let back: Colour = serde_json::from_str(&json).unwrap();
    assert_eq!(c, back);
}

#[test]
fn fill_paint_solid_yaml_roundtrip_bare_hex() {
    let yaml = "'#fafafa'\n";
    let fp: FillPaint = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(matches!(fp, FillPaint::Solid(c) if c == Colour::rgba(0xfa, 0xfa, 0xfa, 0xff)));
    let out = serde_yaml_ng::to_string(&fp).unwrap();
    assert_eq!(out.trim(), "'#fafafa'");
}

#[test]
fn fill_paint_solid_tagged_form_also_parses() {
    let yaml = "kind: solid\ncolour: '#010203'\n";
    let fp: FillPaint = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(matches!(fp, FillPaint::Solid(c) if c == Colour::rgba(1, 2, 3, 0xff)));
}

#[test]
fn fill_paint_hatch_yaml_roundtrip_tagged() {
    let yaml = "kind: hatch\nspacing: 4.0\nangle_deg: 45.0\nline_width: 0.5\ncolour: '#404040'\n";
    let fp: FillPaint = serde_yaml_ng::from_str(yaml).unwrap();
    match fp {
        FillPaint::Hatch {
            spacing,
            angle_deg,
            line_width,
            colour,
        } => {
            assert!((spacing - 4.0).abs() < f32::EPSILON);
            assert!((angle_deg - 45.0).abs() < f32::EPSILON);
            assert!((line_width - 0.5).abs() < f32::EPSILON);
            assert_eq!(colour, Colour::rgba(0x40, 0x40, 0x40, 0xff));
        }
        _ => panic!("expected hatch"),
    }
    let out = serde_yaml_ng::to_string(&fp).unwrap();
    assert!(out.contains("kind: hatch"));
    assert!(out.contains("spacing: 4.0"));
    assert!(out.contains("angle_deg: 45.0"));
    assert!(out.contains("line_width: 0.5"));
    assert!(out.contains("colour: '#404040'"));
}

#[test]
fn fill_paint_image_yaml_roundtrip_tagged() {
    let yaml = "kind: image\nname: brick\n";
    let fp: FillPaint = serde_yaml_ng::from_str(yaml).unwrap();
    match &fp {
        FillPaint::Image { name } => assert_eq!(name, "brick"),
        _ => panic!("expected image"),
    }
    let out = serde_yaml_ng::to_string(&fp).unwrap();
    assert!(out.contains("kind: image"));
    assert!(out.contains("name: brick"));
}

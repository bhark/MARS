#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn pass_of(block: StyleBlock, symbols: &HashMap<String, SymbolDef>) -> SinglePass {
    style_block_to_pass(&block, symbols)
}

#[test]
fn parse_style_block_extracts_color_and_width() {
    let toks = vec![
        Token {
            line: 1,
            keyword: "COLOR".into(),
            args: vec!["255".into(), "0".into(), "0".into()],
        },
        Token {
            line: 2,
            keyword: "WIDTH".into(),
            args: vec!["2.5".into()],
        },
    ];
    let st = parse_style_block(&toks);
    assert_eq!(st.color, Some(Colour::rgb(255, 0, 0)));
    assert_eq!(st.width, Some(2.5));
}

#[test]
fn style_block_to_pass_emits_fill_and_stroke() {
    let p = pass_of(
        StyleBlock {
            color: Some(Colour::rgb(255, 0, 0)),
            outlinecolor: Some(Colour::rgb(0, 0, 0)),
            width: Some(1.0),
            ..Default::default()
        },
        &Default::default(),
    );
    assert_eq!(p.fill, Some(EmitFill::Hex(Colour::rgb(255, 0, 0))));
    assert_eq!(p.stroke, Some(Colour::rgb(0, 0, 0)));
    assert_eq!(p.width, Some(1.0));
    assert!(p.marker.is_none());
}

#[test]
fn style_block_to_pass_resolves_named_circle_symbol_to_marker() {
    let mut symbols = HashMap::new();
    symbols.insert("circle".into(), SymbolDef::Circle);
    let p = pass_of(
        StyleBlock {
            color: Some(Colour::rgb(10, 20, 30)),
            symbol: Some("circle".into()),
            size: Some(8.0),
            ..Default::default()
        },
        &symbols,
    );
    // STYLE.COLOR still emits a solid fill - it's the marker body.
    assert_eq!(p.fill, Some(EmitFill::Hex(Colour::rgb(10, 20, 30))));
    let m = p.marker.unwrap();
    match m {
        EmitMarker::Builtin { kind, size, .. } => {
            assert_eq!(kind, MarkerKind::Circle);
            assert!((size - 8.0).abs() < f32::EPSILON);
        }
        other => panic!("expected builtin marker, got {other:?}"),
    }
}

#[test]
fn style_block_to_pass_resolves_hatch_symbol_to_fill_kind_hatch() {
    let mut symbols = HashMap::new();
    symbols.insert(
        "lines".into(),
        SymbolDef::Hatch {
            angle_deg: Some(45.0),
            size: Some(4.0),
        },
    );
    let p = pass_of(
        StyleBlock {
            color: Some(Colour::rgb(64, 64, 64)),
            width: Some(0.5),
            symbol: Some("lines".into()),
            ..Default::default()
        },
        &symbols,
    );
    match p.fill {
        Some(EmitFill::Hatch {
            spacing,
            angle_deg,
            line_width,
            colour,
        }) => {
            assert!((spacing - 4.0).abs() < f32::EPSILON);
            assert!((angle_deg - 45.0).abs() < f32::EPSILON);
            assert!((line_width - 0.5).abs() < f32::EPSILON);
            assert_eq!(colour, Colour::rgb(64, 64, 64));
        }
        other => panic!("expected hatch fill, got {other:?}"),
    }
    assert!(p.marker.is_none());
}

#[test]
fn parse_style_block_flags_unknown_linejoin() {
    let toks = vec![Token {
        line: 1,
        keyword: "LINEJOIN".into(),
        args: vec!["zigzag".into()],
    }];
    let st = parse_style_block(&toks);
    assert!(st.linejoin.is_none());
    assert_eq!(st.unimplemented, vec!["STYLE.LINEJOIN (unknown value)"]);
}

#[test]
fn parse_style_block_flags_minwidth_maxwidth_once() {
    let toks = vec![
        Token {
            line: 1,
            keyword: "MINWIDTH".into(),
            args: vec!["0.5".into()],
        },
        Token {
            line: 2,
            keyword: "MAXWIDTH".into(),
            args: vec!["5".into()],
        },
        Token {
            line: 3,
            keyword: "MINWIDTH".into(),
            args: vec!["0.25".into()],
        },
    ];
    let st = parse_style_block(&toks);
    assert_eq!(st.unimplemented, vec!["STYLE.MINWIDTH", "STYLE.MAXWIDTH"]);
}

#[test]
fn style_block_to_pass_flags_undefined_symbol() {
    let p = pass_of(
        StyleBlock {
            symbol: Some("ghost".into()),
            ..Default::default()
        },
        &HashMap::new(),
    );
    assert_eq!(p.unimplemented, vec!["STYLE.SYMBOL (undefined)"]);
    assert!(p.marker.is_none());
}

#[test]
fn style_block_to_pass_resolves_pixmap_symbol_to_image_fill() {
    let mut symbols = HashMap::new();
    symbols.insert(
        "brick".into(),
        SymbolDef::Pixmap {
            source_image: Some("/abs/path/to/brick.png".into()),
        },
    );
    let p = pass_of(
        StyleBlock {
            symbol: Some("brick".into()),
            ..Default::default()
        },
        &symbols,
    );
    assert!(p.unimplemented.is_empty(), "PIXMAP no longer surfaces as unimplemented");
    match p.fill {
        Some(EmitFill::Image { name }) => assert_eq!(name, "brick"),
        other => panic!("expected EmitFill::Image, got {other:?}"),
    }
}

#[test]
fn style_block_to_pass_flags_not_implemented_symbol_type_unknown() {
    let mut symbols = HashMap::new();
    symbols.insert(
        "weird".into(),
        SymbolDef::NotImplemented {
            raw_type: "BIZARRO".into(),
        },
    );
    let p = pass_of(
        StyleBlock {
            symbol: Some("weird".into()),
            ..Default::default()
        },
        &symbols,
    );
    assert_eq!(p.unimplemented, vec!["STYLE.SYMBOL (unimplemented type)"]);
}

#[test]
fn style_block_to_pass_flows_angle_into_marker_rotation() {
    let mut symbols = HashMap::new();
    symbols.insert("circle".into(), SymbolDef::Circle);
    let p = pass_of(
        StyleBlock {
            symbol: Some("circle".into()),
            angle_deg: Some(30.0),
            ..Default::default()
        },
        &symbols,
    );
    assert!(p.unimplemented.is_empty(), "marker rotation is supported now");
    match p.marker.unwrap() {
        EmitMarker::Builtin {
            angle: Some(EmitNumeric::Static(a)),
            ..
        } => {
            assert!((a - 30.0).abs() < f32::EPSILON);
        }
        other => panic!("expected builtin marker with static angle, got {other:?}"),
    }
}

#[test]
fn style_block_to_pass_flags_angle_without_marker_or_hatch() {
    let p = pass_of(
        StyleBlock {
            angle_deg: Some(30.0),
            ..Default::default()
        },
        &HashMap::new(),
    );
    assert!(p.marker.is_none());
    assert_eq!(p.unimplemented, vec!["STYLE.ANGLE (non-hatch)"]);
}

#[test]
fn style_block_to_pass_flows_angle_attribute_into_marker() {
    let mut symbols = HashMap::new();
    symbols.insert("circle".into(), SymbolDef::Circle);
    let p = pass_of(
        StyleBlock {
            symbol: Some("circle".into()),
            angle_attribute: Some("bearing".into()),
            ..Default::default()
        },
        &symbols,
    );
    assert!(p.unimplemented.is_empty());
    match p.marker.unwrap() {
        EmitMarker::Builtin {
            angle: Some(EmitNumeric::Attribute(ref col)),
            ..
        } => {
            assert_eq!(col, "bearing");
        }
        other => panic!("expected builtin marker with attribute angle, got {other:?}"),
    }
}

#[test]
fn style_block_to_pass_flows_size_attribute_into_marker() {
    let mut symbols = HashMap::new();
    symbols.insert("circle".into(), SymbolDef::Circle);
    let p = pass_of(
        StyleBlock {
            symbol: Some("circle".into()),
            size_attribute: Some("icon_size".into()),
            ..Default::default()
        },
        &symbols,
    );
    match p.marker.unwrap() {
        EmitMarker::Builtin {
            size_attribute: Some(ref col),
            ..
        } => assert_eq!(col, "icon_size"),
        other => panic!("expected builtin marker with attribute size, got {other:?}"),
    }
}

#[test]
fn style_block_to_pass_does_not_flag_angle_on_hatch() {
    let mut symbols = HashMap::new();
    symbols.insert(
        "lines".into(),
        SymbolDef::Hatch {
            angle_deg: None,
            size: None,
        },
    );
    let p = pass_of(
        StyleBlock {
            symbol: Some("lines".into()),
            angle_deg: Some(45.0),
            ..Default::default()
        },
        &symbols,
    );
    assert!(p.unimplemented.is_empty());
}

#[test]
fn style_block_extracts_symbol_angle_size() {
    let toks = vec![
        Token {
            line: 1,
            keyword: "SYMBOL".into(),
            args: vec!["\"lines\"".into()],
        },
        Token {
            line: 2,
            keyword: "ANGLE".into(),
            args: vec!["30".into()],
        },
        Token {
            line: 3,
            keyword: "SIZE".into(),
            args: vec!["5".into()],
        },
    ];
    let st = parse_style_block(&toks);
    assert_eq!(st.symbol.as_deref(), Some("lines"));
    assert_eq!(st.angle_deg, Some(30.0));
    assert_eq!(st.size, Some(5.0));
}

#[test]
fn parse_style_block_accepts_geomtransform_quoted_and_bare() {
    for raw in ["\"start\"", "start", "Start", "\"VERTICES\""] {
        let toks = vec![Token {
            line: 1,
            keyword: "GEOMTRANSFORM".into(),
            args: vec![raw.into()],
        }];
        let st = parse_style_block(&toks);
        assert!(st.geom_transform.is_some(), "expected match for {raw}");
        assert!(st.unimplemented.is_empty());
    }
}

#[test]
fn parse_style_block_flags_unknown_geomtransform_variant() {
    let toks = vec![Token {
        line: 1,
        keyword: "GEOMTRANSFORM".into(),
        args: vec!["bbox".into()],
    }];
    let st = parse_style_block(&toks);
    assert!(st.geom_transform.is_none());
    assert_eq!(st.unimplemented, vec!["STYLE.GEOMTRANSFORM (unknown variant)"]);
}

#[test]
fn style_block_to_pass_propagates_geom_transform() {
    let p = pass_of(
        StyleBlock {
            geom_transform: Some("vertices"),
            ..Default::default()
        },
        &Default::default(),
    );
    assert_eq!(p.geom_transform, Some("vertices"));
}

#[test]
fn canonical_signature_differs_per_geom_transform() {
    let a = canonical_signature(
        "polygon",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some("start"),
        None,
    );
    let b = canonical_signature(
        "polygon",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some("vertices"),
        None,
    );
    let none = canonical_signature(
        "polygon", None, None, None, None, None, None, None, None, None, None, None, None,
    );
    assert_ne!(a, b);
    assert_ne!(a, none);
}

#[test]
fn canonical_signature_differs_per_min_feature_size() {
    let a = canonical_signature(
        "polygon",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(2.0),
    );
    let b = canonical_signature(
        "polygon",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(8.0),
    );
    let none = canonical_signature(
        "polygon", None, None, None, None, None, None, None, None, None, None, None, None,
    );
    assert_ne!(a, b);
    assert_ne!(a, none);
}

#[test]
fn parse_style_block_accepts_linecap_values() {
    for (raw, expected) in [("butt", "butt"), ("Round", "round"), ("SQUARE", "square")] {
        let toks = vec![Token {
            line: 1,
            keyword: "LINECAP".into(),
            args: vec![raw.into()],
        }];
        let st = parse_style_block(&toks);
        assert_eq!(st.linecap, Some(expected), "raw={raw}");
        assert!(st.unimplemented.is_empty());
    }
}

#[test]
fn parse_style_block_flags_unknown_linecap() {
    let toks = vec![Token {
        line: 1,
        keyword: "LINECAP".into(),
        args: vec!["zigzag".into()],
    }];
    let st = parse_style_block(&toks);
    assert!(st.linecap.is_none());
    assert_eq!(st.unimplemented, vec!["STYLE.LINECAP (unknown value)"]);
}

#[test]
fn style_block_to_pass_propagates_stroke_linecap() {
    let p = pass_of(
        StyleBlock {
            linecap: Some("round"),
            ..Default::default()
        },
        &Default::default(),
    );
    assert_eq!(p.stroke_linecap, Some("round"));
}

#[test]
fn canonical_signature_differs_per_stroke_linecap() {
    let a = canonical_signature(
        "line",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some("butt"),
        None,
        None,
    );
    let b = canonical_signature(
        "line",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some("round"),
        None,
        None,
    );
    let none = canonical_signature(
        "line", None, None, None, None, None, None, None, None, None, None, None, None,
    );
    assert_ne!(a, b);
    assert_ne!(a, none);
}

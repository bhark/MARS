//! STYLE block parser and per-block resolution.
//!
//! A STYLE block (inside CLASS) collects scalar directives into a
//! [`StyleBlock`]. Each parsed block becomes one emitted [`SinglePass`] via
//! [`style_block_to_pass`]; the class-level emitter chooses between a
//! single-pass `Ref` and a multi-pass `Passes` attach. Class-level dedup of
//! the emitted single-pass [`StyleDef`] uses [`canonical_signature`].

use std::collections::HashMap;

use mars_style::Colour;

use crate::directive::StyleDirective;
use crate::emitter::{EmitFill, EmitMarker, EmitStrokeGap, MarkerKind, SymbolDef};
use crate::parsing;
use crate::scanner::Token;

fn push_unique(bag: &mut Vec<&'static str>, name: &'static str) {
    if !bag.contains(&name) {
        bag.push(name);
    }
}

#[derive(Debug, Default)]
pub(crate) struct StyleBlock {
    pub(crate) color: Option<Colour>,
    pub(crate) outlinecolor: Option<Colour>,
    pub(crate) width: Option<f32>,
    pub(crate) outlinewidth: Option<f32>,
    pub(crate) pattern: Option<Vec<f32>>,
    /// STYLE.SYMBOL "<name>" - resolved against Skeleton::symbols at emit
    /// time to decide marker:/fill: { kind: hatch, ... } shape.
    pub(crate) symbol: Option<String>,
    /// STYLE.ANGLE - hatch angle override.
    pub(crate) angle_deg: Option<f32>,
    /// STYLE.SIZE - marker size or hatch spacing override.
    pub(crate) size: Option<f32>,
    /// STYLE.OPACITY <0..100> -> style-wide alpha in [0.0, 1.0].
    pub(crate) opacity: Option<f32>,
    /// STYLE.OFFSET <x> [<y>] -> perpendicular stroke offset in pixels.
    /// mapserver passes (offset_px, -99) for parallel double-strokes; we
    /// honour the first arg as the perpendicular offset.
    pub(crate) offset_px: Option<f32>,
    /// STYLE.GAP <px> + STYLE.INITIALGAP <px> -> stamped marker along path.
    pub(crate) gap_px: Option<f32>,
    pub(crate) initial_gap_px: Option<f32>,
    /// STYLE.LINEJOIN -> mars stroke_linejoin wire value.
    pub(crate) linejoin: Option<&'static str>,
    /// STYLE.GEOMTRANSFORM "<variant>" -> mars geom_transform wire value.
    /// Carries the lowercase wire string ("start" | "end" | "vertices") so
    /// emission stays stringly-typed alongside `linejoin`.
    pub(crate) geom_transform: Option<&'static str>,
    /// Recognised-but-not-implemented STYLE directive names. Aggregated at
    /// resolve time so the parser stays a pure data sink; `emit_layer` fires
    /// one warn per layer summarising what was dropped.
    pub(crate) unimplemented: Vec<&'static str>,
}

pub(crate) fn parse_style_block(body: &[Token]) -> StyleBlock {
    let mut st = StyleBlock::default();
    for t in body {
        match StyleDirective::from_token(t) {
            StyleDirective::Color(t) => st.color = parsing::rgb_triple(t).or(st.color),
            StyleDirective::OutlineColor(t) => st.outlinecolor = parsing::rgb_triple(t).or(st.outlinecolor),
            StyleDirective::Width(t) => st.width = parsing::first_parsed(t).or(st.width),
            StyleDirective::OutlineWidth(t) => st.outlinewidth = parsing::first_parsed(t).or(st.outlinewidth),
            StyleDirective::Pattern(t) => {
                let nums = parsing::nums(t);
                if !nums.is_empty() {
                    st.pattern = Some(nums);
                }
            }
            StyleDirective::Symbol(t) => {
                // STYLE.SYMBOL takes one arg: either a symbol name (string)
                // or a numeric index (legacy). we only resolve named symbols.
                if let Some(s) = parsing::first_unquoted(t)
                    && !s.is_empty()
                    && s.parse::<f64>().is_err()
                {
                    st.symbol = Some(s);
                }
            }
            StyleDirective::Angle(t) => st.angle_deg = parsing::first_parsed(t).or(st.angle_deg),
            StyleDirective::Size(t) => st.size = parsing::first_parsed(t).or(st.size),
            StyleDirective::Opacity(t) => {
                // mapserver OPACITY is 0..100; mars wants 0.0..1.0.
                if let Some(v) = parsing::first_parsed::<f32>(t) {
                    st.opacity = Some((v / 100.0).clamp(0.0, 1.0));
                }
            }
            StyleDirective::Offset(t) => {
                // OFFSET <dx> <dy>: dx is the perpendicular distance; dy is
                // either a real y offset or -99 (mapserver's "parallel
                // double stroke" marker). we honour dx and drop the second
                // arg.
                st.offset_px = parsing::first_parsed(t).or(st.offset_px);
            }
            StyleDirective::Gap(t) => {
                // mapserver: negative gap means "stamp marker along line"
                // with stride |gap|; positive gap is a different sentinel
                // mode. interval_px is unsigned here.
                if let Some(v) = parsing::first_parsed::<f32>(t) {
                    st.gap_px = Some(v.abs());
                }
            }
            StyleDirective::InitialGap(t) => st.initial_gap_px = parsing::first_parsed(t).or(st.initial_gap_px),
            StyleDirective::LineJoin(t) => {
                if let Some(arg) = t.args.first() {
                    match arg.to_ascii_lowercase().as_str() {
                        "miter" => st.linejoin = Some("miter"),
                        "round" => st.linejoin = Some("round"),
                        "bevel" => st.linejoin = Some("bevel"),
                        _ => push_unique(&mut st.unimplemented, "STYLE.LINEJOIN (unknown value)"),
                    }
                }
            }
            StyleDirective::GeomTransform(t) => {
                // mapserver accepts the variant quoted ("start") or bare; the
                // unimplemented bag is the right home for the wider vocabulary
                // (`bbox`, `labelpnt`, `simplify(...)` etc.) until we add it.
                if let Some(arg) = parsing::first_unquoted(t) {
                    match arg.to_ascii_lowercase().as_str() {
                        "start" => st.geom_transform = Some("start"),
                        "end" => st.geom_transform = Some("end"),
                        "vertices" => st.geom_transform = Some("vertices"),
                        _ => push_unique(&mut st.unimplemented, "STYLE.GEOMTRANSFORM (unknown variant)"),
                    }
                }
            }
            StyleDirective::NotImplementedAttenuation(t) => {
                // record the dropped directive as a typed signal; the
                // layer-level warn fires once at emit time.
                let name: &'static str = match t.keyword.to_ascii_uppercase().as_str() {
                    "MINWIDTH" => "STYLE.MINWIDTH",
                    "MAXWIDTH" => "STYLE.MAXWIDTH",
                    _ => "STYLE attenuation",
                };
                push_unique(&mut st.unimplemented, name);
            }
            StyleDirective::Unknown => {}
        }
    }
    st
}

/// Per-block resolution result: one parsed [`StyleBlock`] lowered into the
/// shape a single [`StyleDef`] (or one entry of a `passes:` list) needs. The
/// `unimplemented` bag carries directive names that survived parsing but were
/// dropped during symbol resolution; `resolve_class` merges it (alongside the
/// parser-side `StyleBlock.unimplemented`) into the class-level bag.
#[derive(Debug, Default)]
pub(crate) struct SinglePass {
    pub(crate) fill: Option<EmitFill>,
    pub(crate) stroke: Option<Colour>,
    pub(crate) width: Option<f32>,
    pub(crate) dasharray: Option<Vec<f32>>,
    pub(crate) marker: Option<EmitMarker>,
    pub(crate) opacity: Option<f32>,
    pub(crate) stroke_offset_px: Option<f32>,
    pub(crate) stroke_gap: Option<EmitStrokeGap>,
    pub(crate) stroke_linejoin: Option<&'static str>,
    pub(crate) geom_transform: Option<&'static str>,
    pub(crate) unimplemented: Vec<&'static str>,
}

/// Lower one parsed STYLE block into a [`SinglePass`]. Resolves STYLE.SYMBOL
/// against the mapfile-level symbol table, applies STYLE.COLOR / WIDTH / etc.
/// as overrides, and surfaces dropped-directive signals via `unimplemented`.
/// One block in, one pass out - there is no aggregation across blocks here.
pub(crate) fn style_block_to_pass(s: &StyleBlock, symbols: &HashMap<String, SymbolDef>) -> SinglePass {
    let mut unimplemented: Vec<&'static str> = Vec::new();
    let mut resolved_marker: Option<EmitMarker> = None;
    let mut resolved_hatch: Option<EmitFill> = None;

    if let Some(sym_name) = &s.symbol {
        match symbols.get(sym_name) {
            Some(SymbolDef::Circle) => {
                resolved_marker = Some(EmitMarker::Builtin {
                    kind: MarkerKind::Circle,
                    size: s.size.unwrap_or(6.0),
                });
            }
            Some(SymbolDef::NamedShape(kind)) => {
                resolved_marker = Some(EmitMarker::Builtin {
                    kind: *kind,
                    size: s.size.unwrap_or(6.0),
                });
            }
            Some(SymbolDef::Hatch { angle_deg, size }) => {
                let spacing = s.size.or(*size).unwrap_or(6.0);
                let angle = s.angle_deg.or(*angle_deg).unwrap_or(0.0);
                let line_width = s.width.unwrap_or(1.0);
                let colour = s.color.unwrap_or(Colour::rgb(0, 0, 0));
                resolved_hatch = Some(EmitFill::Hatch {
                    spacing,
                    angle_deg: angle,
                    line_width,
                    colour,
                });
            }
            Some(SymbolDef::VectorShape { points, anchor, filled }) => {
                resolved_marker = Some(EmitMarker::Vector {
                    points: points.clone(),
                    anchor: *anchor,
                    filled: *filled,
                    size: s.size.unwrap_or(6.0),
                });
            }
            Some(SymbolDef::Glyph { font_family, character }) => {
                resolved_marker = Some(EmitMarker::Glyph {
                    font_family: font_family.clone(),
                    character: character.clone(),
                    size: s.size.unwrap_or(12.0),
                });
            }
            Some(SymbolDef::Pixmap { source_image }) => {
                // PIXMAP styles tile the named bitmap. The compiler resolves
                // `name` against `compiler.images_dir` and packs the bytes
                // into the manifest's image_artifact; the runtime renderer
                // then resolves the same name through its `ImageRegistry`.
                // The source_image path on the mapfile is preserved as a
                // one-time hint so the operator knows which file to copy.
                resolved_hatch = Some(EmitFill::Image { name: sym_name.clone() });
                if let Some(p) = source_image {
                    tracing::info!(
                        symbol = sym_name,
                        source_image = %p,
                        "PIXMAP symbol translated to FillPaint::Image; copy the source bitmap into compiler.images_dir as <name>.<ext>"
                    );
                }
            }
            Some(SymbolDef::NotImplemented { raw_type }) => {
                // map known mapfile TYPE keywords to specific bag entries so
                // the operator sees which kind of symbol was dropped.
                let name: &'static str = match raw_type.as_str() {
                    "SVG" => "STYLE.SYMBOL SVG",
                    "OGR" => "STYLE.SYMBOL OGR",
                    _ => "STYLE.SYMBOL (unimplemented type)",
                };
                push_unique(&mut unimplemented, name);
            }
            None => {
                push_unique(&mut unimplemented, "STYLE.SYMBOL (undefined)");
            }
        }
    }

    // ANGLE outside a hatch context is silently ignored at draw time;
    // surface it so the operator sees it in the layer-level warn.
    if resolved_hatch.is_none() && s.angle_deg.is_some() {
        push_unique(&mut unimplemented, "STYLE.ANGLE (non-hatch)");
    }

    let solid_fill = s.color.map(EmitFill::Hex);
    let fill = resolved_hatch.or(solid_fill);
    let stroke = s.outlinecolor;
    let width = s.width.or(s.outlinewidth);
    let dasharray = s.pattern.clone();
    let opacity = s.opacity;
    let stroke_offset_px = s.offset_px;
    let stroke_gap = s.gap_px.map(|gap| EmitStrokeGap {
        interval_px: gap,
        initial_px: s.initial_gap_px.unwrap_or(0.0),
    });
    let stroke_linejoin = s.linejoin;
    let geom_transform = s.geom_transform;

    SinglePass {
        fill,
        stroke,
        width,
        dasharray,
        marker: resolved_marker,
        opacity,
        stroke_offset_px,
        stroke_gap,
        stroke_linejoin,
        geom_transform,
        unimplemented,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn canonical_signature(
    style_type: &str,
    fill: Option<&EmitFill>,
    stroke: Option<&Colour>,
    width: Option<f32>,
    dasharray: Option<&Vec<f32>>,
    marker: Option<&EmitMarker>,
    opacity: Option<f32>,
    stroke_offset_px: Option<f32>,
    stroke_gap: Option<&EmitStrokeGap>,
    stroke_linejoin: Option<&'static str>,
    geom_transform: Option<&'static str>,
) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = write!(s, "kind={style_type}");
    if let Some(f) = fill {
        match f {
            EmitFill::Hex(c) => {
                let _ = write!(s, ",fill={c}");
            }
            EmitFill::Hatch {
                spacing,
                angle_deg,
                line_width,
                colour,
            } => {
                let _ = write!(s, ",hatch=s{spacing},a{angle_deg},w{line_width},c{colour}");
            }
            EmitFill::Image { name } => {
                let _ = write!(s, ",image={name}");
            }
        }
    }
    if let Some(c) = stroke {
        let _ = write!(s, ",stroke={c}");
    }
    if let Some(v) = width {
        let _ = write!(s, ",width={v}");
    }
    if let Some(arr) = dasharray {
        let _ = write!(s, ",dash={arr:?}");
    }
    if let Some(m) = marker {
        match m {
            EmitMarker::Builtin { kind, size } => {
                let _ = write!(s, ",marker={}-{size}", kind.as_wire());
            }
            EmitMarker::Vector {
                points,
                anchor,
                filled,
                size,
            } => {
                let _ = write!(s, ",marker=vec-{filled}-{size}-{points:?}-{anchor:?}");
            }
            EmitMarker::Glyph {
                font_family,
                character,
                size,
            } => {
                let _ = write!(s, ",marker=glyph-{font_family}-{character}-{size}");
            }
        }
    }
    if let Some(o) = opacity {
        let _ = write!(s, ",opacity={o}");
    }
    if let Some(off) = stroke_offset_px {
        let _ = write!(s, ",stroke_offset={off}");
    }
    if let Some(g) = stroke_gap {
        let _ = write!(s, ",stroke_gap=i{},s{}", g.interval_px, g.initial_px);
    }
    if let Some(lj) = stroke_linejoin {
        let _ = write!(s, ",linejoin={lj}");
    }
    if let Some(gt) = geom_transform {
        let _ = write!(s, ",geom_transform={gt}");
    }
    s
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
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
            EmitMarker::Builtin { kind, size } => {
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
    fn style_block_to_pass_flags_angle_on_non_hatch() {
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
        assert!(p.marker.is_some());
        assert_eq!(p.unimplemented, vec!["STYLE.ANGLE (non-hatch)"]);
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
            Some("start"),
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
            Some("vertices"),
        );
        let none = canonical_signature("polygon", None, None, None, None, None, None, None, None, None, None);
        assert_ne!(a, b);
        assert_ne!(a, none);
    }
}

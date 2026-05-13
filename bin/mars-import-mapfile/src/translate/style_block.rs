//! STYLE block parser and resolution helpers.
//!
//! A STYLE block (inside CLASS) collects scalar directives into a
//! [`StyleBlock`]. Multiple STYLE blocks per class are then collapsed into
//! a single resolved fill / stroke / marker / dash / opacity tuple via
//! [`collapse_styles`]. Class-level dedup of the emitted [`StyleDef`] uses
//! [`canonical_signature`].

use std::collections::HashMap;

use mars_style::Colour;
use tracing::warn;

use crate::directive::StyleDirective;
use crate::emitter::{EmitFill, EmitMarker, EmitStrokeGap, MarkerKind, SymbolDef};
use crate::parsing;
use crate::scanner::Token;

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
    /// Directive names recognised but not yet implemented (MINWIDTH, MAXWIDTH).
    /// Aggregated at resolve time so the parser stays a pure data sink;
    /// `emit_layer` fires one warn per layer summarising what was dropped.
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
                        _ => warn!(line = t.line, linejoin = %arg, "unknown STYLE LINEJOIN; dropping"),
                    }
                }
            }
            StyleDirective::NotImplementedAttenuation(t) => {
                // record the dropped directive as a typed signal; the
                // layer-level warn fires once at emit time.
                let name: &'static str = match t.keyword.to_ascii_uppercase().as_str() {
                    "MINWIDTH" => "MINWIDTH",
                    "MAXWIDTH" => "MAXWIDTH",
                    _ => "STYLE attenuation",
                };
                if !st.unimplemented.contains(&name) {
                    st.unimplemented.push(name);
                }
            }
            StyleDirective::Unknown => {}
        }
    }
    st
}

/// outputs of [`collapse_styles`]: a single resolved fill/stroke/width/dash/
/// marker tuple that drives `StyleDef` construction in `parse_class`.
#[derive(Debug, Default)]
pub(crate) struct CollapsedStyle {
    pub(crate) fill: Option<EmitFill>,
    pub(crate) stroke: Option<Colour>,
    pub(crate) width: Option<f32>,
    pub(crate) dasharray: Option<Vec<f32>>,
    pub(crate) marker: Option<EmitMarker>,
    pub(crate) opacity: Option<f32>,
    pub(crate) stroke_offset_px: Option<f32>,
    pub(crate) stroke_gap: Option<EmitStrokeGap>,
    pub(crate) stroke_linejoin: Option<&'static str>,
}

pub(crate) fn collapse_styles(
    styles: &[StyleBlock],
    line: usize,
    symbols: &HashMap<String, SymbolDef>,
) -> CollapsedStyle {
    if styles.len() > 1 {
        warn!(
            line = line,
            count = styles.len(),
            "STYLE: collapsed multi-pass stack to single fill+stroke"
        );
    }
    // resolve a symbol reference into either a marker or a hatch fill,
    // overriding the plain solid fill. ANGLE/SIZE/WIDTH on STYLE take
    // precedence over the symbol's own defaults.
    let mut resolved_marker: Option<EmitMarker> = None;
    let mut resolved_hatch: Option<EmitFill> = None;

    for s in styles {
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
                Some(SymbolDef::NotImplemented { raw_type }) => {
                    // typed signal from parse_symbol: a recognised SYMBOL
                    // block whose TYPE we don't translate yet. warn with the
                    // actual TYPE string so the operator can hand-edit.
                    warn!(
                        line = line,
                        symbol = sym_name.as_str(),
                        raw_type = raw_type.as_str(),
                        "STYLE.SYMBOL TYPE not yet implemented; dropping marker"
                    );
                }
                None => {
                    warn!(
                        line = line,
                        symbol = sym_name.as_str(),
                        "STYLE.SYMBOL references undefined symbol; ignoring"
                    );
                }
            }
        }
    }

    let solid_fill = styles.iter().rev().find_map(|s| s.color).map(EmitFill::Hex);
    let fill = resolved_hatch.or(solid_fill);
    let stroke = styles.iter().find_map(|s| s.outlinecolor);
    let width = styles.iter().find_map(|s| s.width.or(s.outlinewidth));
    let dasharray = styles.iter().find_map(|s| s.pattern.clone());
    let opacity = styles.iter().find_map(|s| s.opacity);
    let stroke_offset_px = styles.iter().find_map(|s| s.offset_px);
    let stroke_gap = styles.iter().find_map(|s| {
        s.gap_px.map(|gap| EmitStrokeGap {
            interval_px: gap,
            initial_px: s.initial_gap_px.unwrap_or(0.0),
        })
    });
    let stroke_linejoin = styles.iter().find_map(|s| s.linejoin);
    CollapsedStyle {
        fill,
        stroke,
        width,
        dasharray,
        marker: resolved_marker,
        opacity,
        stroke_offset_px,
        stroke_gap,
        stroke_linejoin,
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
    s
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

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
    fn collapse_styles_picks_first_fill_and_stroke() {
        let styles = vec![StyleBlock {
            color: Some(Colour::rgb(255, 0, 0)),
            outlinecolor: Some(Colour::rgb(0, 0, 0)),
            width: Some(1.0),
            ..Default::default()
        }];
        let c = collapse_styles(&styles, 1, &Default::default());
        assert_eq!(c.fill, Some(EmitFill::Hex(Colour::rgb(255, 0, 0))));
        assert_eq!(c.stroke, Some(Colour::rgb(0, 0, 0)));
        assert_eq!(c.width, Some(1.0));
        assert!(c.marker.is_none());
    }

    #[test]
    fn collapse_styles_resolves_named_circle_symbol_to_marker() {
        let mut symbols = HashMap::new();
        symbols.insert("circle".into(), SymbolDef::Circle);
        let styles = vec![StyleBlock {
            color: Some(Colour::rgb(10, 20, 30)),
            symbol: Some("circle".into()),
            size: Some(8.0),
            ..Default::default()
        }];
        let c = collapse_styles(&styles, 1, &symbols);
        // STYLE.COLOR still emits a solid fill - it's the marker body.
        assert_eq!(c.fill, Some(EmitFill::Hex(Colour::rgb(10, 20, 30))));
        let m = c.marker.unwrap();
        match m {
            EmitMarker::Builtin { kind, size } => {
                assert_eq!(kind, MarkerKind::Circle);
                assert!((size - 8.0).abs() < f32::EPSILON);
            }
            other => panic!("expected builtin marker, got {other:?}"),
        }
    }

    #[test]
    fn collapse_styles_resolves_hatch_symbol_to_fill_kind_hatch() {
        let mut symbols = HashMap::new();
        symbols.insert(
            "lines".into(),
            SymbolDef::Hatch {
                angle_deg: Some(45.0),
                size: Some(4.0),
            },
        );
        let styles = vec![StyleBlock {
            color: Some(Colour::rgb(64, 64, 64)),
            width: Some(0.5),
            symbol: Some("lines".into()),
            ..Default::default()
        }];
        let c = collapse_styles(&styles, 1, &symbols);
        match c.fill {
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
        assert!(c.marker.is_none());
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
}

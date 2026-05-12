//! style, class, and label parsing for the mapfile translator.

use mars_style::Colour;
use tracing::warn;

use crate::emitter::{
    ClassSkeleton, EmitFill, EmitLinePlacement, EmitMarker, EmitStrokeGap, LabelSkeleton, MarkerKind, Skeleton,
    StyleDef, SymbolDef, slugify,
};
use crate::scanner::{Token, block_range, is_block_opener};
use crate::translate::{is_unsupported, normalize_n_plus_one};

#[derive(Debug, Default)]
struct StyleBlock {
    color: Option<Colour>,
    outlinecolor: Option<Colour>,
    width: Option<f32>,
    outlinewidth: Option<f32>,
    pattern: Option<Vec<f32>>,
    /// STYLE.SYMBOL "<name>" - resolved against Skeleton::symbols at emit
    /// time to decide marker:/fill: { kind: hatch, ... } shape.
    symbol: Option<String>,
    /// STYLE.ANGLE - hatch angle override.
    angle_deg: Option<f32>,
    /// STYLE.SIZE - marker size or hatch spacing override.
    size: Option<f32>,
    /// STYLE.OPACITY <0..100> -> style-wide alpha in [0.0, 1.0].
    opacity: Option<f32>,
    /// STYLE.OFFSET <x> [<y>] -> perpendicular stroke offset in pixels.
    /// mapserver passes (offset_px, -99) for parallel double-strokes; we
    /// honour the first arg as the perpendicular offset.
    offset_px: Option<f32>,
    /// STYLE.GAP <px> + STYLE.INITIALGAP <px> -> stamped marker along path.
    gap_px: Option<f32>,
    initial_gap_px: Option<f32>,
    /// STYLE.LINEJOIN -> mars stroke_linejoin wire value.
    linejoin: Option<&'static str>,
}

pub(crate) fn parse_class(
    body: &[Token],
    class_line: usize,
    layer_name: &str,
    geom_kind: &str,
    skel: &mut Skeleton,
) -> Option<ClassSkeleton> {
    let mut name: Option<String> = None;
    let mut expression: Option<String> = None;
    let mut styles: Vec<StyleBlock> = Vec::new();
    let mut min_scale_denom: Option<u64> = None;
    let mut max_scale_denom: Option<u64> = None;

    let mut i = 0;
    while i < body.len() {
        let t = &body[i];
        let kw = t.keyword.to_ascii_uppercase();
        match kw.as_str() {
            "NAME" if name.is_none() => {
                name = t.args.first().cloned();
                i += 1;
                continue;
            }
            "MINSCALEDENOM" | "MAXSCALEDENOM" => {
                if let Some(arg) = t.args.first() {
                    match arg.parse::<f64>() {
                        Ok(v) if v.is_finite() && v >= 0.0 => {
                            let n = normalize_n_plus_one(v as u64);
                            if kw == "MINSCALEDENOM" {
                                min_scale_denom = Some(n);
                            } else {
                                max_scale_denom = Some(n);
                            }
                        }
                        _ => warn!(line = t.line, keyword = %kw, value = %arg, "could not parse class scale denom"),
                    }
                }
                i += 1;
                continue;
            }
            "EXPRESSION" => {
                let joined = t.args.join(" ");
                match crate::expression::parse_mapfile_expression(&joined, t.line) {
                    Ok(expr) => {
                        expression = Some(format!("{expr}"));
                    }
                    Err(e) => {
                        warn!(line = t.line, error = %e, "could not parse EXPRESSION");
                        expression = Some(format!("# TODO: hand-translate: {joined}"));
                    }
                }
                i += 1;
                continue;
            }
            "STYLE" => {
                if let Some(r) = block_range(body, i) {
                    styles.push(parse_style_block(&body[r.start + 1..r.end - 1]));
                    i = r.end;
                    continue;
                }
            }
            _ => {}
        }
        if is_unsupported(&kw) {
            warn!(line = t.line, keyword = %kw, "unsupported class-level construct");
            if is_block_opener(&kw)
                && let Some(r) = block_range(body, i)
            {
                i = r.end;
                continue;
            }
        }
        i += 1;
    }

    let title = name.clone();
    let class_name = slugify(&name.unwrap_or_else(|| format!("class_l{class_line}")));
    let style_prefix = if geom_kind == "polygon" { "poly" } else { geom_kind };
    let style_name = format!("{}_{}_{}", style_prefix, slugify(layer_name), class_name);

    let collapsed = collapse_styles(&styles, class_line, &skel.symbols);

    let canonical = canonical_signature(
        geom_kind,
        collapsed.fill.as_ref(),
        collapsed.stroke.as_ref(),
        collapsed.width,
        collapsed.dasharray.as_ref(),
        collapsed.marker.as_ref(),
        collapsed.opacity,
        collapsed.stroke_offset_px,
        collapsed.stroke_gap.as_ref(),
        collapsed.stroke_linejoin,
    );

    let existing = skel.styles.iter().find(|s| {
        canonical_signature(
            &s.style_type,
            s.fill.as_ref(),
            s.stroke.as_ref(),
            s.stroke_width,
            s.stroke_dasharray.as_ref(),
            s.marker.as_ref(),
            s.opacity,
            s.stroke_offset_px,
            s.stroke_gap.as_ref(),
            s.stroke_linejoin,
        ) == canonical
    });

    let style_ref = if let Some(st) = existing {
        st.name.clone()
    } else {
        skel.styles.push(StyleDef {
            name: style_name.clone(),
            style_type: geom_kind.to_string(),
            fill: collapsed.fill,
            stroke: collapsed.stroke,
            stroke_width: collapsed.width,
            stroke_dasharray: collapsed.dasharray,
            stroke_linejoin: collapsed.stroke_linejoin,
            marker: collapsed.marker,
            opacity: collapsed.opacity,
            stroke_offset_px: collapsed.stroke_offset_px,
            stroke_gap: collapsed.stroke_gap,
            font_family: None,
            font_size: None,
            halo_color: None,
            halo_width: None,
            priority: None,
            min_distance: None,
        });
        style_name
    };

    Some(ClassSkeleton {
        name: class_name,
        title,
        when: expression,
        min_scale_denom,
        max_scale_denom,
        style_ref,
    })
}

fn parse_style_block(body: &[Token]) -> StyleBlock {
    let mut st = StyleBlock::default();
    for t in body {
        let kw = t.keyword.to_ascii_uppercase();
        match kw.as_str() {
            "COLOR" if t.args.len() >= 3 => {
                if let (Ok(r), Ok(g), Ok(b)) = (t.args[0].parse(), t.args[1].parse(), t.args[2].parse()) {
                    st.color = Some(Colour::rgb(r, g, b));
                }
            }
            "OUTLINECOLOR" if t.args.len() >= 3 => {
                if let (Ok(r), Ok(g), Ok(b)) = (t.args[0].parse(), t.args[1].parse(), t.args[2].parse()) {
                    st.outlinecolor = Some(Colour::rgb(r, g, b));
                }
            }
            "WIDTH" => {
                if let Ok(v) = t.args.first().unwrap_or(&String::new()).parse::<f32>() {
                    st.width = Some(v);
                }
            }
            "OUTLINEWIDTH" => {
                if let Ok(v) = t.args.first().unwrap_or(&String::new()).parse::<f32>() {
                    st.outlinewidth = Some(v);
                }
            }
            "PATTERN" => {
                let nums: Vec<f32> = t.args.iter().filter_map(|a| a.parse().ok()).collect();
                if !nums.is_empty() {
                    st.pattern = Some(nums);
                }
            }
            "SYMBOL" => {
                // STYLE.SYMBOL takes one arg: either a symbol name (string)
                // or a numeric index (legacy). we only resolve named symbols.
                if let Some(name) = t.args.first() {
                    let s = name.trim_matches('"').to_string();
                    if !s.is_empty() && s.parse::<f64>().is_err() {
                        st.symbol = Some(s);
                    }
                }
            }
            "ANGLE" => {
                if let Some(v) = t.args.first().and_then(|a| a.parse::<f32>().ok()) {
                    st.angle_deg = Some(v);
                }
            }
            "SIZE" => {
                if let Some(v) = t.args.first().and_then(|a| a.parse::<f32>().ok()) {
                    st.size = Some(v);
                }
            }
            "OPACITY" => {
                // mapserver OPACITY is 0..100; mars wants 0.0..1.0.
                if let Some(v) = t.args.first().and_then(|a| a.parse::<f32>().ok()) {
                    st.opacity = Some((v / 100.0).clamp(0.0, 1.0));
                }
            }
            "OFFSET" => {
                // OFFSET <dx> <dy>: dx is the perpendicular distance; dy is
                // either a real y offset or -99 (mapserver's "parallel
                // double stroke" marker). we honour dx and drop the second
                // arg.
                if let Some(v) = t.args.first().and_then(|a| a.parse::<f32>().ok()) {
                    st.offset_px = Some(v);
                }
            }
            "GAP" => {
                if let Some(v) = t.args.first().and_then(|a| a.parse::<f32>().ok()) {
                    // mapserver: negative gap means "stamp marker along
                    // line" with stride |gap|; positive gap is a different
                    // sentinel mode. interval_px is unsigned here.
                    st.gap_px = Some(v.abs());
                }
            }
            "INITIALGAP" => {
                if let Some(v) = t.args.first().and_then(|a| a.parse::<f32>().ok()) {
                    st.initial_gap_px = Some(v);
                }
            }
            "LINEJOIN" => {
                if let Some(arg) = t.args.first() {
                    match arg.to_ascii_lowercase().as_str() {
                        "miter" => st.linejoin = Some("miter"),
                        "round" => st.linejoin = Some("round"),
                        "bevel" => st.linejoin = Some("bevel"),
                        _ => warn!(line = t.line, linejoin = %arg, "unknown STYLE LINEJOIN; dropping"),
                    }
                }
            }
            "MINWIDTH" | "MAXWIDTH" => {
                warn!(line = t.line, keyword = %kw, "STYLE {kw} not yet implemented; dropping");
            }
            _ => {}
        }
    }
    st
}

/// outputs of [`collapse_styles`]: a single resolved fill/stroke/width/dash/
/// marker tuple that drives `StyleDef` construction in [`parse_class`].
#[derive(Debug, Default)]
struct CollapsedStyle {
    fill: Option<EmitFill>,
    stroke: Option<Colour>,
    width: Option<f32>,
    dasharray: Option<Vec<f32>>,
    marker: Option<EmitMarker>,
    opacity: Option<f32>,
    stroke_offset_px: Option<f32>,
    stroke_gap: Option<EmitStrokeGap>,
    stroke_linejoin: Option<&'static str>,
}

fn collapse_styles(
    styles: &[StyleBlock],
    line: usize,
    symbols: &std::collections::HashMap<String, SymbolDef>,
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
fn canonical_signature(
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

pub(crate) fn parse_label(
    body: &[Token],
    _line: usize,
    layer_name: &str,
    skel: &mut Skeleton,
) -> Option<LabelSkeleton> {
    let mut text: Option<String> = None;
    let mut font: Option<String> = None;
    let mut size: Option<f32> = None;
    let mut color: Option<Colour> = None;
    let mut outlinecolor: Option<Colour> = None;
    let mut outlinewidth: Option<f32> = None;
    let mut priority: Option<u16> = None;
    let mut min_distance: Option<f32> = None;
    let mut placement_line: Option<EmitLinePlacement> = None;

    // builds the placement_line on demand; line-shape LABEL fields (ANGLE
    // FOLLOW, REPEATDISTANCE, MAXOVERLAPANGLE) all flow into the same struct.
    fn ensure_line(p: &mut Option<EmitLinePlacement>) -> &mut EmitLinePlacement {
        p.get_or_insert(EmitLinePlacement {
            repeat_m: None,
            max_angle_delta_deg: None,
        })
    }

    for t in body {
        let kw = t.keyword.to_ascii_uppercase();
        match kw.as_str() {
            "TEXT" if text.is_none() => text = t.args.first().cloned(),
            "FONT" if font.is_none() => font = t.args.first().cloned(),
            "SIZE" => size = t.args.first().and_then(|a| a.parse().ok()),
            "COLOR" if t.args.len() >= 3 => {
                if let (Ok(r), Ok(g), Ok(b)) = (t.args[0].parse(), t.args[1].parse(), t.args[2].parse()) {
                    color = Some(Colour::rgb(r, g, b));
                }
            }
            "OUTLINECOLOR" if t.args.len() >= 3 => {
                if let (Ok(r), Ok(g), Ok(b)) = (t.args[0].parse(), t.args[1].parse(), t.args[2].parse()) {
                    outlinecolor = Some(Colour::rgb(r, g, b));
                }
            }
            "OUTLINEWIDTH" => {
                outlinewidth = t.args.first().and_then(|a| a.parse().ok());
            }
            "PRIORITY" => {
                if let Some(v) = t.args.first().and_then(|a| a.parse::<i64>().ok()) {
                    // mapserver PRIORITY is 1..=10 by convention; mars allows
                    // any u16. clamp to a sane range.
                    priority = Some(v.clamp(0, u16::MAX as i64) as u16);
                }
            }
            "MINDISTANCE" => {
                if let Some(v) = t.args.first().and_then(|a| a.parse::<f32>().ok()) {
                    min_distance = Some(v);
                }
            }
            "REPEATDISTANCE" => {
                if let Some(v) = t.args.first().and_then(|a| a.parse::<f64>().ok()) {
                    ensure_line(&mut placement_line).repeat_m = Some(v);
                }
            }
            "MAXOVERLAPANGLE" => {
                if let Some(v) = t.args.first().and_then(|a| a.parse::<f32>().ok()) {
                    ensure_line(&mut placement_line).max_angle_delta_deg = Some(v);
                }
            }
            "ANGLE" => {
                if let Some(arg) = t.args.first() {
                    match arg.to_ascii_uppercase().as_str() {
                        "FOLLOW" => {
                            // mark placement as line; sampling defaults kick
                            // in when repeat is unset.
                            ensure_line(&mut placement_line);
                        }
                        "AUTO" => warn!(line = t.line, "LABEL ANGLE AUTO is not yet implemented; dropping"),
                        other => {
                            warn!(line = t.line, value = %other, "LABEL ANGLE numeric values are not yet implemented; dropping")
                        }
                    }
                }
            }
            "POSITION" => warn!(line = t.line, "LABEL POSITION is not yet implemented; dropping"),
            "PARTIALS" => warn!(line = t.line, "LABEL PARTIALS is not yet implemented; dropping"),
            "OFFSET" => warn!(line = t.line, "LABEL OFFSET is not yet implemented; dropping"),
            "TYPE" => {
                if let Some(arg) = t.args.first() {
                    let up = arg.to_ascii_uppercase();
                    if up == "BITMAP" {
                        warn!(
                            line = t.line,
                            "LABEL TYPE BITMAP is not yet implemented; falling back to TrueType"
                        );
                    }
                }
            }
            _ => {}
        }
    }

    // empty text is kept so handle_layer can fill it in from LABELITEM. when
    // neither TEXT nor LABELITEM is set we still emit the LabelSkeleton so
    // style/placement state isn't lost; the operator gets a clean empty
    // `text:` slot to fill in.
    let text = text.unwrap_or_default();
    let style_name = format!("label_{}", slugify(layer_name));
    let fill = color.unwrap_or(Colour::rgb(0, 0, 0));
    // label styles are not deduped against geometry styles
    skel.styles.push(StyleDef {
        name: style_name.clone(),
        style_type: "label".into(),
        fill: Some(EmitFill::Hex(fill)),
        stroke: None,
        stroke_width: None,
        stroke_dasharray: None,
        stroke_linejoin: None,
        marker: None,
        opacity: None,
        stroke_offset_px: None,
        stroke_gap: None,
        font_family: font.or_else(|| Some("sans-serif".into())),
        font_size: size.or(Some(12.0)),
        halo_color: outlinecolor,
        halo_width: outlinewidth,
        priority,
        min_distance,
    });

    Some(LabelSkeleton {
        text,
        style_ref: style_name,
        placement_line,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::scanner::Token;

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
        let mut symbols = std::collections::HashMap::new();
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
        let mut symbols = std::collections::HashMap::new();
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

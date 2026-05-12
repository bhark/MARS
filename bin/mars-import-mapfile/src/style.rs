//! style, class, and label parsing for the mapfile translator.

use tracing::warn;

use crate::emitter::{ClassSkeleton, EmitFill, EmitMarker, LabelSkeleton, Skeleton, StyleDef, SymbolDef, rgb_to_hex, slugify};
use crate::scanner::{Token, block_range, is_block_opener};
use crate::translate::{is_unsupported, normalize_n_plus_one};

#[derive(Debug, Default)]
struct StyleBlock {
    color: Option<(u8, u8, u8)>,
    outlinecolor: Option<(u8, u8, u8)>,
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
    );

    let existing = skel.styles.iter().find(|s| {
        canonical_signature(
            &s.style_type,
            s.fill.as_ref(),
            s.stroke.as_ref(),
            s.stroke_width,
            s.stroke_dasharray.as_ref(),
            s.marker.as_ref(),
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
            marker: collapsed.marker,
            font_family: None,
            font_size: None,
            halo_color: None,
            halo_width: None,
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
                    st.color = Some((r, g, b));
                }
            }
            "OUTLINECOLOR" if t.args.len() >= 3 => {
                if let (Ok(r), Ok(g), Ok(b)) = (t.args[0].parse(), t.args[1].parse(), t.args[2].parse()) {
                    st.outlinecolor = Some((r, g, b));
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
    stroke: Option<String>,
    width: Option<f32>,
    dasharray: Option<Vec<f32>>,
    marker: Option<EmitMarker>,
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
                    resolved_marker = Some(EmitMarker {
                        kind: "circle",
                        size: s.size.unwrap_or(6.0),
                    });
                }
                Some(SymbolDef::NamedShape(kind)) => {
                    if let Some(k) = match kind.as_str() {
                        "circle" => Some("circle"),
                        "square" => Some("square"),
                        "triangle" => Some("triangle"),
                        "cross" => Some("cross"),
                        "x" => Some("x"),
                        "pin" => Some("pin"),
                        _ => None,
                    } {
                        resolved_marker = Some(EmitMarker {
                            kind: k,
                            size: s.size.unwrap_or(6.0),
                        });
                    } else {
                        warn!(
                            line = line,
                            symbol = sym_name.as_str(),
                            shape = kind.as_str(),
                            "STYLE.SYMBOL references unrecognised named shape; ignoring"
                        );
                    }
                }
                Some(SymbolDef::Hatch { angle_deg, size }) => {
                    let spacing = s.size.or(*size).unwrap_or(6.0);
                    let angle = s.angle_deg.or(*angle_deg).unwrap_or(0.0);
                    let line_width = s.width.unwrap_or(1.0);
                    let colour = s
                        .color
                        .map(|(r, g, b)| rgb_to_hex(r, g, b))
                        .unwrap_or_else(|| "#000000".into());
                    resolved_hatch = Some(EmitFill::Hatch {
                        spacing,
                        angle_deg: angle,
                        line_width,
                        colour,
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

    let solid_fill = styles
        .iter()
        .rev()
        .find_map(|s| s.color)
        .map(|(r, g, b)| rgb_to_hex(r, g, b))
        .map(EmitFill::Hex);
    let fill = resolved_hatch.or(solid_fill);
    let stroke = styles
        .iter()
        .find_map(|s| s.outlinecolor)
        .map(|(r, g, b)| rgb_to_hex(r, g, b));
    let width = styles.iter().find_map(|s| s.width.or(s.outlinewidth));
    let dasharray = styles.iter().find_map(|s| s.pattern.clone());
    CollapsedStyle {
        fill,
        stroke,
        width,
        dasharray,
        marker: resolved_marker,
    }
}

fn canonical_signature(
    style_type: &str,
    fill: Option<&EmitFill>,
    stroke: Option<&String>,
    width: Option<f32>,
    dasharray: Option<&Vec<f32>>,
    marker: Option<&EmitMarker>,
) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = write!(s, "kind={style_type}");
    if let Some(f) = fill {
        match f {
            EmitFill::Hex(h) => {
                let _ = write!(s, ",fill={h}");
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
    if let Some(v) = stroke {
        let _ = write!(s, ",stroke={v}");
    }
    if let Some(v) = width {
        let _ = write!(s, ",width={v}");
    }
    if let Some(arr) = dasharray {
        let _ = write!(s, ",dash={arr:?}");
    }
    if let Some(m) = marker {
        let _ = write!(s, ",marker={}-{}", m.kind, m.size);
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
    let mut color: Option<(u8, u8, u8)> = None;
    let mut outlinecolor: Option<(u8, u8, u8)> = None;
    let mut outlinewidth: Option<f32> = None;

    for t in body {
        let kw = t.keyword.to_ascii_uppercase();
        match kw.as_str() {
            "TEXT" if text.is_none() => text = t.args.first().cloned(),
            "FONT" if font.is_none() => font = t.args.first().cloned(),
            "SIZE" => size = t.args.first().and_then(|a| a.parse().ok()),
            "COLOR" if t.args.len() >= 3 => {
                if let (Ok(r), Ok(g), Ok(b)) = (t.args[0].parse(), t.args[1].parse(), t.args[2].parse()) {
                    color = Some((r, g, b));
                }
            }
            "OUTLINECOLOR" if t.args.len() >= 3 => {
                if let (Ok(r), Ok(g), Ok(b)) = (t.args[0].parse(), t.args[1].parse(), t.args[2].parse()) {
                    outlinecolor = Some((r, g, b));
                }
            }
            "OUTLINEWIDTH" => {
                outlinewidth = t.args.first().and_then(|a| a.parse().ok());
            }
            _ => {}
        }
    }

    let text = text?;
    let style_name = format!("label_{}", slugify(layer_name));
    let fill = color
        .map(|(r, g, b)| rgb_to_hex(r, g, b))
        .unwrap_or_else(|| "#000000".into());
    // label styles are not deduped against geometry styles
    skel.styles.push(StyleDef {
        name: style_name.clone(),
        style_type: "label".into(),
        fill: Some(EmitFill::Hex(fill)),
        stroke: None,
        stroke_width: None,
        stroke_dasharray: None,
        marker: None,
        font_family: font.or_else(|| Some("sans-serif".into())),
        font_size: size.or(Some(12.0)),
        halo_color: outlinecolor.map(|(r, g, b)| rgb_to_hex(r, g, b)),
        halo_width: outlinewidth,
    });

    Some(LabelSkeleton {
        text,
        style_ref: style_name,
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
        assert_eq!(st.color, Some((255, 0, 0)));
        assert_eq!(st.width, Some(2.5));
    }

    #[test]
    fn collapse_styles_picks_first_fill_and_stroke() {
        let styles = vec![StyleBlock {
            color: Some((255, 0, 0)),
            outlinecolor: Some((0, 0, 0)),
            width: Some(1.0),
            outlinewidth: None,
            pattern: None,
            symbol: None,
            angle_deg: None,
            size: None,
        }];
        let c = collapse_styles(&styles, 1, &Default::default());
        assert_eq!(c.fill, Some(EmitFill::Hex("#ff0000".into())));
        assert_eq!(c.stroke, Some("#000000".into()));
        assert_eq!(c.width, Some(1.0));
        assert!(c.marker.is_none());
    }

    #[test]
    fn collapse_styles_resolves_named_circle_symbol_to_marker() {
        let mut symbols = std::collections::HashMap::new();
        symbols.insert("circle".into(), SymbolDef::Circle);
        let styles = vec![StyleBlock {
            color: Some((10, 20, 30)),
            symbol: Some("circle".into()),
            size: Some(8.0),
            ..Default::default()
        }];
        let c = collapse_styles(&styles, 1, &symbols);
        // STYLE.COLOR still emits a solid fill - it's the marker body.
        assert_eq!(c.fill, Some(EmitFill::Hex("#0a141e".into())));
        let m = c.marker.unwrap();
        assert_eq!(m.kind, "circle");
        assert!((m.size - 8.0).abs() < f32::EPSILON);
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
            color: Some((64, 64, 64)),
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
                assert_eq!(colour, "#404040");
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

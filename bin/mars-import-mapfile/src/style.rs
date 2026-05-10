//! style, class, and label parsing for the mapfile translator.

use tracing::warn;

use crate::emitter::{ClassSkeleton, LabelSkeleton, Skeleton, StyleDef, rgb_to_hex, slugify};
use crate::scanner::{Token, block_range, is_block_opener};
use crate::translate::{is_unsupported, normalize_n_plus_one};

#[derive(Debug, Default)]
struct StyleBlock {
    color: Option<(u8, u8, u8)>,
    outlinecolor: Option<(u8, u8, u8)>,
    width: Option<f32>,
    outlinewidth: Option<f32>,
    pattern: Option<Vec<f32>>,
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

    let (fill, stroke, stroke_width, dasharray) = collapse_styles(&styles, class_line);

    // dedupe identical styles
    let canonical = {
        let mut tmp = String::new();
        let _ = std::fmt::write(&mut tmp, format_args!("kind={geom_kind}"));
        if let Some(ref v) = fill {
            let _ = std::fmt::write(&mut tmp, format_args!(",fill={v}"));
        }
        if let Some(ref v) = stroke {
            let _ = std::fmt::write(&mut tmp, format_args!(",stroke={v}"));
        }
        if let Some(v) = stroke_width {
            let _ = std::fmt::write(&mut tmp, format_args!(",width={v}"));
        }
        if let Some(ref arr) = dasharray {
            let _ = std::fmt::write(&mut tmp, format_args!(",dash={arr:?}"));
        }
        tmp
    };

    let existing = skel.styles.iter().find(|s| {
        let mut tmp = String::new();
        let _ = std::fmt::write(&mut tmp, format_args!("kind={}", s.style_type));
        if let Some(ref v) = s.fill {
            let _ = std::fmt::write(&mut tmp, format_args!(",fill={v}"));
        }
        if let Some(ref v) = s.stroke {
            let _ = std::fmt::write(&mut tmp, format_args!(",stroke={v}"));
        }
        if let Some(v) = s.stroke_width {
            let _ = std::fmt::write(&mut tmp, format_args!(",width={v}"));
        }
        if let Some(ref arr) = s.stroke_dasharray {
            let _ = std::fmt::write(&mut tmp, format_args!(",dash={arr:?}"));
        }
        tmp == canonical
    });

    let style_ref = if let Some(st) = existing {
        st.name.clone()
    } else {
        skel.styles.push(StyleDef {
            name: style_name.clone(),
            style_type: geom_kind.to_string(),
            fill,
            stroke,
            stroke_width,
            stroke_dasharray: dasharray,
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
            _ => {}
        }
    }
    st
}

fn collapse_styles(
    styles: &[StyleBlock],
    line: usize,
) -> (Option<String>, Option<String>, Option<f32>, Option<Vec<f32>>) {
    if styles.len() > 1 {
        warn!(
            line = line,
            count = styles.len(),
            "STYLE: collapsed multi-pass stack to single fill+stroke"
        );
    }
    let fill = styles
        .iter()
        .rev()
        .find_map(|s| s.color)
        .map(|(r, g, b)| rgb_to_hex(r, g, b));
    let stroke = styles
        .iter()
        .find_map(|s| s.outlinecolor)
        .map(|(r, g, b)| rgb_to_hex(r, g, b));
    let width = styles.iter().find_map(|s| s.width.or(s.outlinewidth));
    let dasharray = styles.iter().find_map(|s| s.pattern.clone());
    (fill, stroke, width, dasharray)
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
        fill: Some(fill),
        stroke: None,
        stroke_width: None,
        stroke_dasharray: None,
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
        }];
        let (fill, stroke, width, _dash) = collapse_styles(&styles, 1);
        assert_eq!(fill, Some("#ff0000".into()));
        assert_eq!(stroke, Some("#000000".into()));
        assert_eq!(width, Some(1.0));
    }
}

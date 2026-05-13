//! LABEL block parser. Walk tokens, accumulate a [`ParsedLabel`] bag of
//! `Option` fields. No defaulting, no emit - defaults live in
//! [`super::resolved`]; emit lives in [`super::emit`].

use mars_style::Colour;

use crate::directive::LabelDirective;
use crate::emitter::EmitLinePlacement;
use crate::parsing;
use crate::scanner::Token;

#[derive(Debug, Default)]
pub(crate) struct ParsedLabel {
    pub text: Option<String>,
    pub font: Option<String>,
    pub size: Option<f32>,
    pub color: Option<Colour>,
    pub outlinecolor: Option<Colour>,
    pub outlinewidth: Option<f32>,
    pub priority: Option<u16>,
    pub min_distance: Option<f32>,
    pub placement_line: Option<EmitLinePlacement>,
    /// Recognised-but-not-implemented LABEL directive names. Aggregated at
    /// resolve time into the layer-level bag; `emit_layer` fires one warn
    /// summarising what was dropped.
    pub unimplemented: Vec<&'static str>,
}

fn push_unique(bag: &mut Vec<&'static str>, name: &'static str) {
    if !bag.contains(&name) {
        bag.push(name);
    }
}

pub(crate) fn parse_label(body: &[Token]) -> ParsedLabel {
    let mut p = ParsedLabel::default();

    // builds the placement_line on demand; line-shape LABEL fields (ANGLE
    // FOLLOW, REPEATDISTANCE, MAXOVERLAPANGLE) all flow into the same struct.
    fn ensure_line(p: &mut Option<EmitLinePlacement>) -> &mut EmitLinePlacement {
        p.get_or_insert(EmitLinePlacement {
            repeat_m: None,
            max_angle_delta_deg: None,
        })
    }

    for t in body {
        match LabelDirective::from_token(t) {
            LabelDirective::Text(t) if p.text.is_none() => p.text = t.args.first().cloned(),
            LabelDirective::Font(t) if p.font.is_none() => p.font = t.args.first().cloned(),
            LabelDirective::Size(t) => p.size = parsing::first_parsed(t),
            LabelDirective::Color(t) => p.color = parsing::rgb_triple(t).or(p.color),
            LabelDirective::OutlineColor(t) => p.outlinecolor = parsing::rgb_triple(t).or(p.outlinecolor),
            LabelDirective::OutlineWidth(t) => p.outlinewidth = parsing::first_parsed(t),
            LabelDirective::Priority(t) => {
                // mapserver PRIORITY is 1..=10 by convention; mars allows any
                // u16. clamp to a sane range.
                if let Some(v) = parsing::first_parsed::<i64>(t) {
                    p.priority = Some(v.clamp(0, u16::MAX as i64) as u16);
                }
            }
            LabelDirective::MinDistance(t) => {
                if let Some(v) = parsing::first_parsed::<f32>(t) {
                    p.min_distance = Some(v);
                }
            }
            LabelDirective::RepeatDistance(t) => {
                if let Some(v) = parsing::first_parsed::<f64>(t) {
                    ensure_line(&mut p.placement_line).repeat_m = Some(v);
                }
            }
            LabelDirective::MaxOverlapAngle(t) => {
                if let Some(v) = parsing::first_parsed::<f32>(t) {
                    ensure_line(&mut p.placement_line).max_angle_delta_deg = Some(v);
                }
            }
            LabelDirective::Angle(t) => {
                if let Some(arg) = t.args.first() {
                    match arg.to_ascii_uppercase().as_str() {
                        "FOLLOW" => {
                            // mark placement as line; sampling defaults kick
                            // in when repeat is unset.
                            ensure_line(&mut p.placement_line);
                        }
                        "AUTO" => push_unique(&mut p.unimplemented, "LABEL.ANGLE AUTO"),
                        _ => push_unique(&mut p.unimplemented, "LABEL.ANGLE (numeric)"),
                    }
                }
            }
            LabelDirective::NotImplemented(t) => {
                let name: &'static str = match t.keyword.to_ascii_uppercase().as_str() {
                    "POSITION" => "LABEL.POSITION",
                    "PARTIALS" => "LABEL.PARTIALS",
                    "OFFSET" => "LABEL.OFFSET",
                    "TYPE" => match t.args.first() {
                        Some(arg) if arg.eq_ignore_ascii_case("BITMAP") => "LABEL.TYPE BITMAP",
                        _ => "LABEL.TYPE (unimplemented)",
                    },
                    _ => "LABEL directive (unimplemented)",
                };
                push_unique(&mut p.unimplemented, name);
            }
            // re-occurrence of TEXT / FONT after the first is ignored; same
            // for any keyword we don't understand inside a LABEL block.
            LabelDirective::Text(_) | LabelDirective::Font(_) | LabelDirective::Unknown => {}
        }
    }

    p
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn tok(keyword: &str, args: &[&str]) -> Token {
        Token {
            line: 1,
            keyword: keyword.into(),
            args: args.iter().map(|s| (*s).into()).collect(),
        }
    }

    #[test]
    fn parse_label_flags_angle_auto() {
        let p = parse_label(&[tok("ANGLE", &["AUTO"])]);
        assert_eq!(p.unimplemented, vec!["LABEL.ANGLE AUTO"]);
    }

    #[test]
    fn parse_label_flags_angle_numeric() {
        let p = parse_label(&[tok("ANGLE", &["45"])]);
        assert_eq!(p.unimplemented, vec!["LABEL.ANGLE (numeric)"]);
    }

    #[test]
    fn parse_label_angle_follow_does_not_flag() {
        let p = parse_label(&[tok("ANGLE", &["FOLLOW"])]);
        assert!(p.unimplemented.is_empty());
        assert!(p.placement_line.is_some());
    }

    #[test]
    fn parse_label_flags_position_partials_offset() {
        let p = parse_label(&[
            tok("POSITION", &["UC"]),
            tok("PARTIALS", &["TRUE"]),
            tok("OFFSET", &["2", "3"]),
        ]);
        assert_eq!(
            p.unimplemented,
            vec!["LABEL.POSITION", "LABEL.PARTIALS", "LABEL.OFFSET"]
        );
    }

    #[test]
    fn parse_label_flags_type_bitmap() {
        let p = parse_label(&[tok("TYPE", &["BITMAP"])]);
        assert_eq!(p.unimplemented, vec!["LABEL.TYPE BITMAP"]);
    }

    #[test]
    fn parse_label_dedups_repeat_directives() {
        let p = parse_label(&[tok("POSITION", &["UC"]), tok("POSITION", &["UL"])]);
        assert_eq!(p.unimplemented, vec!["LABEL.POSITION"]);
    }
}

//! LABEL block parser. Walk tokens, accumulate a [`ParsedLabel`] bag of
//! `Option` fields. No defaulting, no emit - defaults live in
//! [`super::resolved`]; emit lives in [`super::emit`].

use mars_style::{AnchorPosition, Colour, LineAngleMode};

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
    pub position: Option<AnchorPosition>,
    pub offset_px: Option<(f32, f32)>,
    pub angle_deg: Option<f32>,
    /// `[col]` form on LABEL.ANGLE - the label resolves rotation from this
    /// attribute at render time. Mutually exclusive with `angle_deg`.
    pub angle_attribute: Option<String>,
    pub partials: Option<bool>,
    pub force: Option<bool>,
    /// Recognised-but-not-implemented LABEL directive names. Aggregated at
    /// resolve time into the layer-level bag; `emit_layer` fires one warn
    /// summarising what was dropped.
    pub unimplemented: Vec<&'static str>,
}

fn parse_position(arg: &str) -> Option<AnchorPosition> {
    match arg.to_ascii_uppercase().as_str() {
        "UL" => Some(AnchorPosition::Ul),
        "UC" => Some(AnchorPosition::Uc),
        "UR" => Some(AnchorPosition::Ur),
        "CL" => Some(AnchorPosition::Cl),
        "CC" => Some(AnchorPosition::Cc),
        "CR" => Some(AnchorPosition::Cr),
        "LL" => Some(AnchorPosition::Ll),
        "LC" => Some(AnchorPosition::Lc),
        "LR" => Some(AnchorPosition::Lr),
        "AUTO" => Some(AnchorPosition::Auto),
        _ => None,
    }
}

fn parse_bool(arg: &str) -> Option<bool> {
    match arg.to_ascii_uppercase().as_str() {
        "TRUE" | "ON" | "1" => Some(true),
        "FALSE" | "OFF" | "0" => Some(false),
        _ => None,
    }
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
            angle_mode: None,
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
                if let Some(col) = parsing::bracketed_ident(t) {
                    p.angle_attribute = Some(col);
                } else if let Some(arg) = t.args.first() {
                    match arg.to_ascii_uppercase().as_str() {
                        "FOLLOW" => {
                            // mark placement as line + per-character orient.
                            ensure_line(&mut p.placement_line).angle_mode = Some(LineAngleMode::Follow);
                        }
                        "AUTO" => {
                            // line-block orient at sample tangent; default
                            // for line layers, but be explicit so a later
                            // default flip doesn't surprise.
                            ensure_line(&mut p.placement_line).angle_mode = Some(LineAngleMode::Auto);
                        }
                        _ => {
                            // numeric degrees -> static label rotation.
                            if let Ok(v) = arg.parse::<f32>() {
                                p.angle_deg = Some(v);
                            }
                        }
                    }
                }
            }
            LabelDirective::Position(t) => {
                if let Some(arg) = t.args.first()
                    && let Some(pos) = parse_position(arg)
                {
                    p.position = Some(pos);
                }
            }
            LabelDirective::Offset(t) => {
                // OFFSET dx dy - both required.
                if t.args.len() >= 2
                    && let (Some(dx), Some(dy)) = (t.args[0].parse::<f32>().ok(), t.args[1].parse::<f32>().ok())
                {
                    p.offset_px = Some((dx, dy));
                }
            }
            LabelDirective::Partials(t) => {
                if let Some(arg) = t.args.first()
                    && let Some(b) = parse_bool(arg)
                {
                    p.partials = Some(b);
                }
            }
            LabelDirective::Force(t) => {
                if let Some(arg) = t.args.first()
                    && let Some(b) = parse_bool(arg)
                {
                    p.force = Some(b);
                }
            }
            LabelDirective::NotImplemented(t) => {
                let name: &'static str = match t.keyword.to_ascii_uppercase().as_str() {
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
mod tests;

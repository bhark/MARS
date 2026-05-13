//! LABEL block parser. Walks a label body and emits both a `label_*`
//! [`StyleDef`] (font / halo / priority) and a [`LabelSkeleton`] (text +
//! optional line placement).

use mars_style::Colour;
use tracing::warn;

use crate::directive::LabelDirective;
use crate::emitter::{EmitFill, EmitLinePlacement, LabelSkeleton, Skeleton, StyleDef, slugify};
use crate::parsing;
use crate::scanner::Token;

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
        match LabelDirective::from_token(t) {
            LabelDirective::Text(t) if text.is_none() => text = t.args.first().cloned(),
            LabelDirective::Font(t) if font.is_none() => font = t.args.first().cloned(),
            LabelDirective::Size(t) => size = parsing::first_parsed(t),
            LabelDirective::Color(t) => color = parsing::rgb_triple(t).or(color),
            LabelDirective::OutlineColor(t) => outlinecolor = parsing::rgb_triple(t).or(outlinecolor),
            LabelDirective::OutlineWidth(t) => outlinewidth = parsing::first_parsed(t),
            LabelDirective::Priority(t) => {
                // mapserver PRIORITY is 1..=10 by convention; mars allows any
                // u16. clamp to a sane range.
                if let Some(v) = parsing::first_parsed::<i64>(t) {
                    priority = Some(v.clamp(0, u16::MAX as i64) as u16);
                }
            }
            LabelDirective::MinDistance(t) => {
                if let Some(v) = parsing::first_parsed::<f32>(t) {
                    min_distance = Some(v);
                }
            }
            LabelDirective::RepeatDistance(t) => {
                if let Some(v) = parsing::first_parsed::<f64>(t) {
                    ensure_line(&mut placement_line).repeat_m = Some(v);
                }
            }
            LabelDirective::MaxOverlapAngle(t) => {
                if let Some(v) = parsing::first_parsed::<f32>(t) {
                    ensure_line(&mut placement_line).max_angle_delta_deg = Some(v);
                }
            }
            LabelDirective::Angle(t) => {
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
            LabelDirective::NotImplemented(t) => {
                let kw = t.keyword.to_ascii_uppercase();
                if kw == "TYPE" {
                    if let Some(arg) = t.args.first()
                        && arg.eq_ignore_ascii_case("BITMAP")
                    {
                        warn!(
                            line = t.line,
                            "LABEL TYPE BITMAP is not yet implemented; falling back to TrueType"
                        );
                    }
                } else {
                    warn!(line = t.line, "LABEL {kw} is not yet implemented; dropping");
                }
            }
            // re-occurrence of TEXT / FONT after the first is ignored; same
            // for any keyword we don't understand inside a LABEL block.
            LabelDirective::Text(_) | LabelDirective::Font(_) | LabelDirective::Unknown => {}
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

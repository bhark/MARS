//! ResolvedLabel + label-text template lowering. Owns the `[col]` -> `{col}`
//! bracket-to-brace translation and the small style-name helpers used by
//! sibling resolve modules.

use std::collections::BTreeSet;

use mars_style::Colour;

use crate::emitter::{EmitLinePlacement, slugify};

use super::super::label::ParsedLabel;

#[derive(Debug)]
pub(crate) struct ResolvedLabel {
    pub text: String,
    pub style_name: String,
    pub fill: Colour,
    pub font_family: String,
    pub font_size: f32,
    pub halo_color: Option<Colour>,
    pub halo_width: Option<f32>,
    pub priority: Option<u16>,
    pub min_distance: Option<f32>,
    pub placement_line: Option<EmitLinePlacement>,
    pub position: Option<mars_style::AnchorPosition>,
    pub offset_px: Option<(f32, f32)>,
    pub angle_deg: Option<f32>,
    pub angle_attribute: Option<String>,
    pub partials: Option<bool>,
    pub force: Option<bool>,
    pub unimplemented: Vec<&'static str>,
}

pub(super) fn resolve_label(p: ParsedLabel, style_name: &str, label_item: Option<&str>) -> ResolvedLabel {
    // LABELITEM: if the LABEL block had no TEXT, the layer's labelitem
    // becomes a `{<col>}` template referencing the column. when neither
    // TEXT nor LABELITEM is set we leave text empty so the operator gets a
    // clean `text:` slot to fill in. Explicit TEXT args go through
    // [`mapfile_text_to_template`] so MapServer's `[col]` column-ref form
    // (and the `(expr)` wrapper) lowers into MARS's `{col}` template form.
    let text = match (p.text.filter(|s| !s.is_empty()), label_item) {
        (Some(t), _) => mapfile_text_to_template(&t),
        (None, Some(item)) => format!("{{{item}}}"),
        (None, None) => String::new(),
    };

    ResolvedLabel {
        text,
        style_name: style_name.to_string(),
        fill: p.color.unwrap_or(Colour::rgb(0, 0, 0)),
        font_family: p.font.unwrap_or_else(|| "sans-serif".into()),
        font_size: p.size.unwrap_or(12.0),
        halo_color: p.outlinecolor,
        halo_width: p.outlinewidth,
        priority: p.priority,
        min_distance: p.min_distance,
        placement_line: p.placement_line,
        position: p.position,
        offset_px: p.offset_px,
        angle_deg: p.angle_deg,
        angle_attribute: p.angle_attribute,
        partials: p.partials,
        force: p.force,
        unimplemented: p.unimplemented,
    }
}

pub(super) fn layer_label_style_name(layer: &str) -> String {
    format!("label_{}", slugify(layer))
}

pub(super) fn class_label_style_name(layer: &str, class: &str) -> String {
    format!("label_{}__{}", slugify(layer), class)
}

pub(super) fn collect_template_idents(text: &str, out: &mut BTreeSet<String>) {
    if let Ok(t) = mars_expr::parse_template(text) {
        for seg in &t.segments {
            if let mars_expr::Segment::Ident(name) = seg {
                out.insert(name.clone());
            }
        }
    }
}

// Lower a mapfile LABEL TEXT arg into a MARS template string. Recognises:
// `[col]` column refs -> `{col}`, and a single `(expr)` wrapper -> strip
// outer parens (mapfile expression form). Anything else passes through
// verbatim. The translation is intentionally minimal; complex expressions
// like `(tostring([col],"%fmt"))` stay verbatim so the operator notices.
fn mapfile_text_to_template(raw: &str) -> String {
    let trimmed = raw.trim();
    let stripped = if trimmed.starts_with('(') && trimmed.ends_with(')') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    bracket_refs_to_braces(stripped)
}

fn bracket_refs_to_braces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '[' {
            out.push(c);
            continue;
        }
        // peek for an ident-shaped run terminated by ']'. fall back to
        // verbatim on anything else so we never turn unrelated bracket
        // syntax into a malformed template.
        let mut ident = String::new();
        let mut closed = false;
        while let Some(&nc) = chars.peek() {
            if nc == ']' {
                chars.next();
                closed = true;
                break;
            }
            if nc.is_ascii_alphanumeric() || nc == '_' {
                ident.push(nc);
                chars.next();
            } else {
                break;
            }
        }
        if closed && !ident.is_empty() {
            out.push('{');
            out.push_str(&ident);
            out.push('}');
        } else {
            out.push('[');
            out.push_str(&ident);
            if closed {
                out.push(']');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;

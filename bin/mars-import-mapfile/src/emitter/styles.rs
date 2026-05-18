//! class / style / marker / label body YAML writers shared between the
//! `styles:` registry and inline pass lists.

use std::fmt::Write as _;

use super::skeleton::{ClassSkeleton, ClassStyleAttach, LabelSkeleton};
use super::style_model::{EmitFill, EmitMarker, EmitNumeric, StyleDef, blend_mode_yaml, line_angle_mode_yaml};
use super::yaml::{quote_colour, yaml_quote};

/// render one class entry under `    classes:`. compact flow-mapping when the
/// class has no per-class label and the style is a single `Ref`; expanded
/// block-mapping when there's a label or the style is a multi-pass `Passes`
/// list (which never fits cleanly in flow form).
pub(super) fn write_class(out: &mut String, cls: &ClassSkeleton) {
    let mut parts = vec![format!("name: {}", yaml_quote(&cls.name))];
    if let Some(title) = &cls.title {
        parts.push(format!("title: {}", yaml_quote(title)));
    }
    if let Some(when) = &cls.when {
        parts.push(format!("when: {}", yaml_quote(when)));
    }
    if cls.min_scale_denom.is_some() || cls.max_scale_denom.is_some() {
        let mut scale_parts: Vec<String> = Vec::new();
        if let Some(m) = cls.min_scale_denom {
            scale_parts.push(format!("min: {m}"));
        }
        if let Some(m) = cls.max_scale_denom {
            scale_parts.push(format!("max: {m}"));
        }
        parts.push(format!("scale: {{ {} }}", scale_parts.join(", ")));
    }

    let style_is_ref = matches!(cls.style, ClassStyleAttach::Ref(_));
    if let ClassStyleAttach::Ref(name) = &cls.style {
        parts.push(format!("style: {{ type: ref, name: {} }}", yaml_quote(name)));
    }

    // compact flow form: single-ref style, no per-class label.
    if cls.label.is_none() && style_is_ref {
        let _ = writeln!(out, "      - {{ {} }}", parts.join(", "));
        return;
    }

    // expanded form: scalars (and the style ref if any) on their own lines;
    // then either the inline passes block, the per-class label block, or both.
    let mut iter = parts.into_iter();
    if let Some(first) = iter.next() {
        let _ = writeln!(out, "      - {first}");
    }
    for p in iter {
        let _ = writeln!(out, "        {p}");
    }
    if let ClassStyleAttach::Passes(passes) = &cls.style {
        write_inline_passes(out, passes, "        ");
    }
    if let Some(lbl) = cls.label.as_ref() {
        let _ = writeln!(out, "        label:");
        write_label_body(out, lbl, "          ");
    }
}

/// Render an inline `style: { type: passes, passes: [...] }` block at
/// `indent`. Each pass is a [`StyleDef`] body written as a YAML map.
fn write_inline_passes(out: &mut String, passes: &[StyleDef], indent: &str) {
    let _ = writeln!(out, "{indent}style:");
    let _ = writeln!(out, "{indent}  type: passes");
    let _ = writeln!(out, "{indent}  passes:");
    let item_indent = format!("{indent}    ");
    let body_indent = format!("{indent}      ");
    for pass in passes {
        let _ = writeln!(out, "{item_indent}-");
        write_geometry_style_body(out, pass, &body_indent);
    }
}

/// Render the wire-form body of a single geometry [`StyleDef`] at `indent`.
/// Shared between the standalone `styles:` registry entry and the per-pass
/// items inside a `passes:` list. Label-typed entries are not emitted here -
/// they always live in the registry.
pub(super) fn write_geometry_style_body(out: &mut String, st: &StyleDef, indent: &str) {
    match &st.fill {
        Some(EmitFill::Hex(c)) => {
            let _ = writeln!(out, "{indent}fill: {}", quote_colour(*c));
        }
        Some(EmitFill::Hatch {
            spacing,
            angle_deg,
            line_width,
            colour,
        }) => {
            let _ = writeln!(
                out,
                "{indent}fill: {{ kind: hatch, spacing: {spacing}, angle_deg: {angle_deg}, line_width: {line_width}, colour: {} }}",
                quote_colour(*colour)
            );
        }
        Some(EmitFill::Image { name }) => {
            let _ = writeln!(out, "{indent}fill: {{ kind: image, name: {} }}", yaml_quote(name));
        }
        None => {}
    }
    if let Some(c) = st.stroke {
        let _ = writeln!(out, "{indent}stroke: {}", quote_colour(c));
    }
    if let Some(v) = st.stroke_width {
        let _ = writeln!(out, "{indent}stroke_width: {v}");
    }
    if let Some(ref arr) = st.stroke_dasharray {
        let _ = writeln!(
            out,
            "{indent}stroke_dasharray: [{}]",
            arr.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(", ")
        );
    }
    if let Some(lj) = st.stroke_linejoin {
        let _ = writeln!(out, "{indent}stroke_linejoin: {lj}");
    }
    if let Some(lc) = st.stroke_linecap {
        let _ = writeln!(out, "{indent}stroke_linecap: {lc}");
    }
    if let Some(o) = st.opacity {
        let _ = writeln!(out, "{indent}opacity: {o}");
    }
    if let Some(off) = st.stroke_offset_px {
        let _ = writeln!(out, "{indent}stroke_offset_px: {off}");
    }
    if let Some(g) = st.stroke_gap {
        let _ = writeln!(
            out,
            "{indent}stroke_gap: {{ interval_px: {}, initial_px: {} }}",
            g.interval_px, g.initial_px
        );
    }
    if let Some(gt) = st.geom_transform {
        let _ = writeln!(out, "{indent}geom_transform: {gt}");
    }
    if let Some(t) = st.min_feature_size_px {
        let _ = writeln!(out, "{indent}min_feature_size_px: {t}");
    }
    if let Some(bm) = st.blend_mode {
        let _ = writeln!(out, "{indent}blend_mode: {}", blend_mode_yaml(bm));
    }
    if let Some(m) = st.marker.as_ref() {
        write_marker_at(out, m, indent);
    }
}

/// Render a marker at the given indent. Used by both the registry emitter
/// (`    marker: ...`) and the inline-passes path (deeper indent).
fn write_marker_at(out: &mut String, m: &EmitMarker, indent: &str) {
    match m {
        EmitMarker::Builtin {
            kind,
            size,
            size_attribute,
            angle,
        } => {
            let size_v = size_field_wire(*size, size_attribute.as_deref());
            let angle_field = angle_wire_field(angle.as_ref());
            let _ = writeln!(
                out,
                "{indent}marker: {{ kind: {}, size: {size_v}{angle_field} }}",
                kind.as_wire()
            );
        }
        EmitMarker::Glyph {
            font_family,
            character,
            size,
            size_attribute,
            angle,
        } => {
            let size_v = size_field_wire(*size, size_attribute.as_deref());
            let angle_field = angle_wire_field(angle.as_ref());
            let _ = writeln!(
                out,
                "{indent}marker: {{ kind: glyph, font_family: {}, character: {}, size: {size_v}{angle_field} }}",
                yaml_quote(font_family),
                yaml_quote(character)
            );
        }
        EmitMarker::Vector {
            points,
            anchor,
            filled,
            size,
            size_attribute,
            angle,
        } => {
            let _ = writeln!(out, "{indent}marker:");
            let _ = writeln!(out, "{indent}  kind: vector_shape");
            let pts = points
                .iter()
                .map(|(x, y)| format!("[{x}, {y}]"))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(out, "{indent}  points: [{pts}]");
            if let Some((ax, ay)) = anchor {
                let _ = writeln!(out, "{indent}  anchor: [{ax}, {ay}]");
            }
            let _ = writeln!(out, "{indent}  filled: {filled}");
            let size_v = size_field_wire(*size, size_attribute.as_deref());
            let _ = writeln!(out, "{indent}  size: {size_v}");
            if let Some(a) = angle {
                let _ = writeln!(out, "{indent}  angle: {}", numeric_wire(a));
            }
        }
    }
}

fn size_field_wire(size_px: f32, attr: Option<&str>) -> String {
    match attr {
        Some(col) => format!("\"[{col}]\""),
        None => size_px.to_string(),
    }
}

fn angle_wire_field(angle: Option<&EmitNumeric>) -> String {
    match angle {
        Some(a) => format!(", angle: {}", numeric_wire(a)),
        None => String::new(),
    }
}

fn numeric_wire(n: &EmitNumeric) -> String {
    match n {
        EmitNumeric::Static(v) => v.to_string(),
        EmitNumeric::Attribute(col) => format!("\"[{col}]\""),
    }
}

/// render a [`LabelSkeleton`] under an externally-emitted `label:` key at
/// `indent`. shared between the layer-level and class-level label paths.
pub(super) fn write_label_body(out: &mut String, lbl: &LabelSkeleton, indent: &str) {
    let _ = writeln!(out, "{indent}text: {}", yaml_quote(&lbl.text));
    let _ = writeln!(
        out,
        "{indent}style: {{ type: ref, name: {} }}",
        yaml_quote(&lbl.style_ref)
    );
    if let Some(p) = lbl.placement_line {
        let mut parts = vec!["kind: line".to_string()];
        if let Some(r) = p.repeat_m {
            parts.push(format!("repeat_m: {r}"));
        }
        if let Some(a) = p.max_angle_delta_deg {
            parts.push(format!("max_angle_delta_deg: {a}"));
        }
        if let Some(m) = p.angle_mode {
            parts.push(format!("angle_mode: {}", line_angle_mode_yaml(m)));
        }
        let _ = writeln!(out, "{indent}placement: {{ {} }}", parts.join(", "));
    }
}

//! YAML string-shape primitives shared by the per-block writers.

use mars_style::Colour;

/// slugify a name for YAML identifiers: lowercase, non-alnum → '_'.
pub(crate) fn slugify(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

/// quote a YAML string using simple double-quoting; escapes `"` and `\`.
pub(super) fn yaml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// quote a `Colour` as a YAML string (`"#rrggbb"` or `"#rrggbbaa"`).
pub(super) fn quote_colour(c: Colour) -> String {
    yaml_quote(&c.to_string())
}

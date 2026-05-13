//! token-argument parsing helpers shared by every per-block parser.

use std::str::FromStr;

use mars_style::Colour;

use crate::scanner::Token;

pub(crate) fn first(t: &Token) -> Option<&str> {
    t.args.first().map(String::as_str)
}

pub(crate) fn first_parsed<T: FromStr>(t: &Token) -> Option<T> {
    first(t).and_then(|s| s.parse().ok())
}

pub(crate) fn first_unquoted(t: &Token) -> Option<String> {
    first(t).map(|s| s.trim_matches('"').to_string())
}

/// `COLOR r g b` / `OUTLINECOLOR r g b`. mapserver accepts integer 0..=255
/// channels; non-integer or short arg lists yield `None`.
pub(crate) fn rgb_triple(t: &Token) -> Option<Colour> {
    if t.args.len() < 3 {
        return None;
    }
    let r = t.args[0].parse().ok()?;
    let g = t.args[1].parse().ok()?;
    let b = t.args[2].parse().ok()?;
    Some(Colour::rgb(r, g, b))
}

/// Flatten all args parseable as `f32`. Used by PATTERN (dasharray) and the
/// flattened VECTOR POINTS body, where unparseable tokens are silently
/// dropped to match mapserver's lenient numeric scanning.
pub(crate) fn nums(t: &Token) -> Vec<f32> {
    t.args.iter().filter_map(|a| a.parse().ok()).collect()
}

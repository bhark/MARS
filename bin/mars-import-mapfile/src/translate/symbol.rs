//! SYMBOL block parser. Resolves the mapfile SYMBOL types we support into
//! the [`SymbolDef`] vocabulary the emitter reads.

use crate::directive::SymbolDirective;
use crate::emitter::{MarkerKind, SymbolDef};
use crate::parsing;
use crate::scanner::{Token, block_range};

/// parse a mapfile SYMBOL definition body into a `SymbolDef`. recognises:
///
/// - TYPE ELLIPSE -> Circle
/// - TYPE HATCH -> Hatch (with ANGLE/SIZE defaults)
/// - TYPE VECTOR with POINTS body -> VectorShape (filled / anchored)
/// - TYPE VECTOR without POINTS but with a known shape NAME -> NamedShape
/// - TYPE TRUETYPE -> Glyph (FONT + CHARACTER)
///
/// other TYPEs (PIXMAP) are dropped with a warn at use site.
pub(crate) fn parse_symbol(body: &[Token]) -> Option<(String, SymbolDef)> {
    let mut name: Option<String> = None;
    let mut type_: Option<String> = None;
    let mut angle_deg: Option<f32> = None;
    let mut size: Option<f32> = None;
    let mut points: Vec<(f32, f32)> = Vec::new();
    let mut filled = false;
    let mut anchor: Option<(f32, f32)> = None;
    let mut font: Option<String> = None;
    let mut character: Option<String> = None;
    let mut i = 0;
    while i < body.len() {
        let t = &body[i];
        match SymbolDirective::from_token(t) {
            SymbolDirective::Name(t) if name.is_none() => name = t.args.first().cloned(),
            SymbolDirective::Type(t) if type_.is_none() => type_ = t.args.first().cloned(),
            SymbolDirective::Angle(t) => angle_deg = parsing::first_parsed(t),
            SymbolDirective::Size(t) => size = parsing::first_parsed(t),
            SymbolDirective::Filled(t) => {
                if let Some(arg) = t.args.first() {
                    filled = matches!(arg.to_ascii_uppercase().as_str(), "TRUE" | "ON" | "YES" | "1");
                }
            }
            SymbolDirective::Points(t) => {
                // POINTS is a block; coords land on the inner tokens. each
                // inner token has the first coord as `keyword` and the rest
                // as `args`. flatten all numerics and group into (x, y) pairs.
                if let Some(r) = block_range(body, i) {
                    let mut coords: Vec<f32> = Vec::new();
                    for inner in &body[r.start + 1..r.end - 1] {
                        if let Ok(v) = inner.keyword.parse::<f32>() {
                            coords.push(v);
                        }
                        coords.extend(parsing::nums(inner));
                    }
                    for pair in coords.chunks_exact(2) {
                        points.push((pair[0], pair[1]));
                    }
                    i = r.end;
                    continue;
                }
                // POINTS without an END: read the (possibly inline) coord
                // list off the current token's args.
                for pair in parsing::nums(t).chunks_exact(2) {
                    points.push((pair[0], pair[1]));
                }
            }
            SymbolDirective::AnchorPoint(t) => {
                let coords = parsing::nums(t);
                if coords.len() >= 2 {
                    anchor = Some((coords[0], coords[1]));
                }
            }
            SymbolDirective::Font(t) => font = parsing::first_unquoted(t),
            SymbolDirective::Character(t) => character = parsing::first_unquoted(t),
            // re-occurrence of NAME / TYPE after the first is ignored; same
            // for any keyword we don't understand inside a SYMBOL block.
            SymbolDirective::Name(_) | SymbolDirective::Type(_) | SymbolDirective::Unknown => {}
        }
        i += 1;
    }
    let name = name?.trim_matches('"').to_string();
    let type_up = type_.unwrap_or_default().to_ascii_uppercase();
    let def = match type_up.as_str() {
        "ELLIPSE" => SymbolDef::Circle,
        "HATCH" => SymbolDef::Hatch { angle_deg, size },
        "VECTOR" => {
            if !points.is_empty() {
                SymbolDef::VectorShape { points, anchor, filled }
            } else {
                SymbolDef::NamedShape(MarkerKind::from_lowercase(&name.to_ascii_lowercase())?)
            }
        }
        "TRUETYPE" => SymbolDef::Glyph {
            font_family: font.unwrap_or_else(|| "sans-serif".to_string()),
            character: character?,
        },
        other => SymbolDef::NotImplemented {
            raw_type: other.to_string(),
        },
    };
    Some((name, def))
}

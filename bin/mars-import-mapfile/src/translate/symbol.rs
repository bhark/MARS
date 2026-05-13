//! SYMBOL block parser. Splits into:
//!
//! - [`parse_symbol`] - walk tokens, accumulate a [`ParsedSymbol`] bag of
//!   `Option` fields. No defaulting, no TYPE -> SymbolDef resolution.
//! - [`emit_symbol`] - take a [`ParsedSymbol`] and resolve the mapfile TYPE
//!   into the [`SymbolDef`] vocabulary the emitter reads.

use crate::directive::SymbolDirective;
use crate::emitter::{MarkerKind, SymbolDef};
use crate::parsing;
use crate::scanner::{Token, block_range};

#[derive(Debug, Default)]
pub(crate) struct ParsedSymbol {
    pub name: Option<String>,
    pub type_: Option<String>,
    pub angle_deg: Option<f32>,
    pub size: Option<f32>,
    pub points: Vec<(f32, f32)>,
    pub filled: bool,
    pub anchor: Option<(f32, f32)>,
    pub font: Option<String>,
    pub character: Option<String>,
}

pub(crate) fn parse_symbol(body: &[Token]) -> ParsedSymbol {
    let mut p = ParsedSymbol::default();
    let mut i = 0;
    while i < body.len() {
        let t = &body[i];
        match SymbolDirective::from_token(t) {
            SymbolDirective::Name(t) if p.name.is_none() => p.name = t.args.first().cloned(),
            SymbolDirective::Type(t) if p.type_.is_none() => p.type_ = t.args.first().cloned(),
            SymbolDirective::Angle(t) => p.angle_deg = parsing::first_parsed(t),
            SymbolDirective::Size(t) => p.size = parsing::first_parsed(t),
            SymbolDirective::Filled(t) => {
                if let Some(arg) = t.args.first() {
                    p.filled = matches!(arg.to_ascii_uppercase().as_str(), "TRUE" | "ON" | "YES" | "1");
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
                        p.points.push((pair[0], pair[1]));
                    }
                    i = r.end;
                    continue;
                }
                // POINTS without an END: read the (possibly inline) coord
                // list off the current token's args.
                for pair in parsing::nums(t).chunks_exact(2) {
                    p.points.push((pair[0], pair[1]));
                }
            }
            SymbolDirective::AnchorPoint(t) => {
                let coords = parsing::nums(t);
                if coords.len() >= 2 {
                    p.anchor = Some((coords[0], coords[1]));
                }
            }
            SymbolDirective::Font(t) => p.font = parsing::first_unquoted(t),
            SymbolDirective::Character(t) => p.character = parsing::first_unquoted(t),
            // re-occurrence of NAME / TYPE after the first is ignored; same
            // for any keyword we don't understand inside a SYMBOL block.
            SymbolDirective::Name(_) | SymbolDirective::Type(_) | SymbolDirective::Unknown => {}
        }
        i += 1;
    }
    p
}

/// resolves a parsed SYMBOL block into the typed [`SymbolDef`] vocabulary:
///
/// - TYPE ELLIPSE -> Circle
/// - TYPE HATCH -> Hatch (with ANGLE/SIZE defaults)
/// - TYPE VECTOR with POINTS body -> VectorShape (filled / anchored)
/// - TYPE VECTOR without POINTS but with a known shape NAME -> NamedShape
/// - TYPE TRUETYPE -> Glyph (FONT + CHARACTER)
/// - other recognised TYPEs (PIXMAP) -> NotImplemented, warned at use site.
pub(crate) fn emit_symbol(p: ParsedSymbol) -> Option<(String, SymbolDef)> {
    let name = p.name?.trim_matches('"').to_string();
    let type_up = p.type_.unwrap_or_default().to_ascii_uppercase();
    let def = match type_up.as_str() {
        "ELLIPSE" => SymbolDef::Circle,
        "HATCH" => SymbolDef::Hatch {
            angle_deg: p.angle_deg,
            size: p.size,
        },
        "VECTOR" => {
            if !p.points.is_empty() {
                SymbolDef::VectorShape {
                    points: p.points,
                    anchor: p.anchor,
                    filled: p.filled,
                }
            } else {
                SymbolDef::NamedShape(MarkerKind::from_lowercase(&name.to_ascii_lowercase())?)
            }
        }
        "TRUETYPE" => SymbolDef::Glyph {
            font_family: p.font.unwrap_or_else(|| "sans-serif".to_string()),
            character: p.character?,
        },
        other => SymbolDef::NotImplemented {
            raw_type: other.to_string(),
        },
    };
    Some((name, def))
}

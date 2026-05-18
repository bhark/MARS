//! ResolvedSymbol + the SYMBOL TYPE -> SymbolDef lift. NOT_IMPLEMENTED is
//! the typed fallback so unknown TYPEs survive into the wire format with
//! their raw spelling.

use crate::emitter::{MarkerKind, SymbolDef};

use super::super::symbol::ParsedSymbol;

#[derive(Debug)]
pub(crate) struct ResolvedSymbol {
    pub name: String,
    pub def: SymbolDef,
}

pub(crate) fn resolve_symbol(p: ParsedSymbol) -> Option<ResolvedSymbol> {
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
            font_family: p.font.unwrap_or_else(|| "sans-serif".into()),
            character: p.character?,
        },
        "PIXMAP" => SymbolDef::Pixmap { source_image: p.image },
        other => SymbolDef::NotImplemented {
            raw_type: other.to_string(),
        },
    };
    Some(ResolvedSymbol { name, def })
}

//! mapfile-to-skeleton translation pipeline.
//!
//! Layout follows the per-block-kind shape established for the render
//! adapter (see `docs/EXTENDING.md`):
//!
//! - `mod.rs` (this file) owns the top-level MAP walk and shared helpers
//!   (`is_unsupported`, `normalize_n_plus_one`).
//! - `layer.rs` owns `handle_layer` and the mapfile-DATA -> binding
//!   lifting helpers.
//! - `symbol.rs` owns SYMBOL parsing.

mod class;
mod emit;
mod fontset;
mod label;
mod layer;
mod resolved;
mod style_block;
mod symbol;

use std::collections::HashSet;
use std::path::Path;

use tracing::warn;

use crate::directive::MapDirective;
use crate::emitter::{EmitMarker, Skeleton, SymbolDef};
#[cfg(test)]
use crate::scanner::scan;
use crate::scanner::{Token, block_range, is_block_opener};

use self::emit::emit_symbol;
use self::fontset::FontAliases;
use self::layer::handle_layer;
use self::map_metadata::parse_map_metadata;
use self::resolved::resolve_symbol;
use self::symbol::parse_symbol;

mod layer_metadata;
mod map_metadata;

/// keywords whose presence we don't translate yet. some are block openers,
/// some are scalar directives - `walk` handles both.
///
/// METADATA is intentionally absent: MAP-level METADATA flows through
/// `parse_map_metadata` (service-side OWS keys) and LAYER-level METADATA
/// flows through `parse_layer_metadata` (per-layer WMS keys).
const UNSUPPORTED: &[&str] = &[
    "LEGEND",
    "OUTPUTFORMAT",
    "FEATURE",
    "JOIN",
    "CLUSTER",
    "GRID",
    "VALIDATION",
];

pub(crate) fn is_unsupported(kw: &str) -> bool {
    let up = kw.to_ascii_uppercase();
    UNSUPPORTED.iter().any(|b| *b == up)
}

/// translate a mapfile source into a YAML skeleton, warning on unsupported
/// constructs as a side-effect via `tracing::warn!`. test-only helper; the
/// binary entry point in `main.rs` drives `translate_tokens` directly so it
/// can filter layers.
#[cfg(test)]
fn translate(src: &str) -> Skeleton {
    let tokens = scan(src);
    translate_tokens(&tokens, None, None, false)
}

pub(crate) fn translate_tokens(
    tokens: &[Token],
    include_layers: Option<&HashSet<String>>,
    base_dir: Option<&Path>,
    strict: bool,
) -> Skeleton {
    let mut skel = Skeleton::default();

    let map_slice: &[Token] = match tokens
        .iter()
        .position(|t| t.keyword.eq_ignore_ascii_case("MAP"))
        .and_then(|i| block_range(tokens, i))
    {
        Some(r) => &tokens[r.start + 1..r.end.saturating_sub(1).max(r.start + 1)],
        None => tokens,
    };

    let aliases = resolve_fontset(map_slice, base_dir);
    walk(map_slice, &mut skel, include_layers, strict);
    apply_font_aliases(&mut skel, &aliases);
    skel
}

/// scan the MAP body for a FONTSET directive and load its alias table. when
/// `base_dir` is absent or the path is unresolvable, returns an empty map -
/// the test-only `translate(src)` helper drives this path.
fn resolve_fontset(map_slice: &[Token], base_dir: Option<&Path>) -> FontAliases {
    let Some(path_str) = map_slice
        .iter()
        .find(|t| t.keyword.eq_ignore_ascii_case("FONTSET"))
        .and_then(|t| t.args.first())
    else {
        return FontAliases::default();
    };
    let Some(base) = base_dir else {
        // FONTSET seen but no anchor to resolve against; warn and skip.
        warn!(path = %path_str, "FONTSET referenced without a mapfile base dir; aliases will pass through verbatim");
        return FontAliases::default();
    };
    let resolved = base.join(path_str);
    fontset::load(&resolved)
}

/// rewrite every alias reference in the skeleton to its resolved family name.
/// no-op when the alias table is empty or no reference matches an entry.
fn apply_font_aliases(skel: &mut Skeleton, aliases: &FontAliases) {
    if aliases.is_empty() {
        return;
    }
    for style in &mut skel.styles {
        if let Some(family) = &style.font_family
            && let Some(resolved) = aliases.resolve(family)
        {
            style.font_family = Some(resolved.to_string());
        }
        if let Some(EmitMarker::Glyph { font_family, .. }) = &mut style.marker
            && let Some(resolved) = aliases.resolve(font_family)
        {
            *font_family = resolved.to_string();
        }
    }
    for def in skel.symbols.values_mut() {
        if let SymbolDef::Glyph { font_family, .. } = def
            && let Some(resolved) = aliases.resolve(font_family)
        {
            *font_family = resolved.to_string();
        }
    }
}

fn walk(tokens: &[Token], skel: &mut Skeleton, include_layers: Option<&HashSet<String>>, strict: bool) {
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        match MapDirective::from_token(t, is_unsupported) {
            MapDirective::Name(t) if skel.service_name.is_none() => {
                if let Some(v) = t.args.first() {
                    skel.service_name = Some(v.clone());
                }
            }
            MapDirective::Title(t) if skel.service_title.is_none() => {
                if let Some(v) = t.args.first() {
                    skel.service_title = Some(v.clone());
                }
            }
            MapDirective::Layer(open) => {
                let range = block_range(tokens, i).unwrap_or(i..i + 1);
                let body: &[Token] = if range.end > range.start + 1 {
                    &tokens[range.start + 1..range.end - 1]
                } else {
                    &[]
                };
                handle_layer(body, open.line, skel, include_layers, strict);
                i = range.end;
                continue;
            }
            MapDirective::Symbol => {
                let range = block_range(tokens, i).unwrap_or(i..i + 1);
                let body: &[Token] = if range.end > range.start + 1 {
                    &tokens[range.start + 1..range.end - 1]
                } else {
                    &[]
                };
                if let Some(resolved) = resolve_symbol(parse_symbol(body)) {
                    emit_symbol(resolved, skel);
                }
                i = range.end;
                continue;
            }
            MapDirective::Metadata => {
                let range = block_range(tokens, i).unwrap_or(i..i + 1);
                let body: &[Token] = if range.end > range.start + 1 {
                    &tokens[range.start + 1..range.end - 1]
                } else {
                    &[]
                };
                parse_map_metadata(body, &mut skel.service_meta);
                i = range.end;
                continue;
            }
            MapDirective::Projection(_t) => {
                let range = block_range(tokens, i).unwrap_or(i..i + 1);
                let body: &[Token] = if range.end > range.start + 1 {
                    &tokens[range.start + 1..range.end - 1]
                } else {
                    &[]
                };
                if skel.map_projection.is_none()
                    && let Some(crs) = parse_projection_block(body)
                {
                    skel.map_projection = Some(crs);
                }
                i = range.end;
                continue;
            }
            MapDirective::MaxSize(t) => {
                if let Some(n) = parse_map_u32(t) {
                    skel.wms_max_image_dimension = Some(n);
                }
            }
            MapDirective::Resolution(t) => {
                if let Some(v) = parse_map_positive_f64(t) {
                    skel.scale_dpi = Some(v);
                }
            }
            // fontset is resolved up front in `translate_tokens`; absorb the
            // token here so the unknown-keyword path doesn't trip on it.
            MapDirective::Fontset => {}
            MapDirective::Unsupported(t) => {
                warn!(line = t.line, keyword = %t.keyword, "unsupported mapfile construct");
                if is_block_opener(&t.keyword)
                    && let Some(r) = block_range(tokens, i)
                {
                    i = r.end;
                    continue;
                }
            }
            // re-occurrence of NAME / TITLE after the first wins-once rule
            // is ignored; same for keywords we don't understand at top level.
            MapDirective::Name(_) | MapDirective::Title(_) | MapDirective::Unknown => {}
        }
        i += 1;
    }
}

/// Parse a `PROJECTION { ... }` block body into an `EPSG:NNNN` CRS code.
/// Only the `init=epsg:NNNN` form is recognised (case-insensitive); raw
/// `+proj=...` parameter lists return `None`. Each line of the block scans
/// as one token whose keyword is the first arg and whose later args (if any)
/// are siblings; we look across both keyword and args for the init=epsg
/// fragment.
pub(crate) fn parse_projection_block(body: &[Token]) -> Option<String> {
    for t in body {
        if let Some(code) = init_epsg_from(&t.keyword) {
            return Some(code);
        }
        for arg in &t.args {
            if let Some(code) = init_epsg_from(arg) {
                return Some(code);
            }
        }
    }
    None
}

fn init_epsg_from(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    let rest = lower.strip_prefix("init=epsg:")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    Some(format!("EPSG:{digits}"))
}

/// parse a MAP-scope scalar that should be a non-negative u32 (e.g. MAXSIZE).
/// warns at the token's line on bad input and returns None.
fn parse_map_u32(t: &Token) -> Option<u32> {
    let arg = t.args.first()?;
    match arg.parse::<u32>() {
        Ok(n) => Some(n),
        Err(_) => {
            warn!(line = t.line, keyword = %t.keyword, value = %arg, "could not parse as u32");
            None
        }
    }
}

/// parse a MAP-scope scalar that should be a finite, strictly-positive f64
/// (e.g. RESOLUTION). warns at the token's line on bad input and returns None.
fn parse_map_positive_f64(t: &Token) -> Option<f64> {
    let arg = t.args.first()?;
    match arg.parse::<f64>() {
        Ok(v) if v.is_finite() && v > 0.0 => Some(v),
        _ => {
            warn!(line = t.line, keyword = %t.keyword, value = %arg, "could not parse as positive f64");
            None
        }
    }
}

/// canonicalize MapServer's `MINSCALEDENOM = N+1` half-open convention.
/// when `n - 1` lands cleanly on a "round" base (10000, 5000, 1000, 500, 100),
/// snap down. conservative - values not on a round base are left alone.
pub(crate) fn normalize_n_plus_one(n: u64) -> u64 {
    if n <= 1 {
        return n;
    }
    const BASES: &[u64] = &[10_000, 5_000, 1_000, 500, 100];
    for &base in BASES {
        if (n - 1) >= base && (n - 1).is_multiple_of(base) {
            return n - 1;
        }
    }
    n
}

#[cfg(test)]
mod tests;

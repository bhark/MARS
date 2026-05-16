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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::layer::{LiftedBinding, lift_inline_subquery};
    use super::*;
    use crate::emitter::MarkerKind;

    #[test]
    fn translate_extracts_name_title_and_layers() {
        let src = r#"
MAP
  NAME "demo"
  TITLE "Demo Service"
  LAYER
    NAME "roads"
    TYPE LINE
  END
  LAYER
    NAME "buildings"
    TYPE POLYGON
  END
END
"#;
        let skel = translate(src);
        assert_eq!(skel.service_name.as_deref(), Some("demo"));
        assert_eq!(skel.service_title.as_deref(), Some("Demo Service"));
        let names: Vec<&str> = skel.layers.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["roads", "buildings"]);
    }

    #[test]
    fn translate_extracts_classes_and_sources() {
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "roads"
    TYPE LINE
    DATA "geometri FROM roads_table"
    CLASS
      NAME "main"
      EXPRESSION ([type] = 'main')
      STYLE
        COLOR 190 190 190
        WIDTH 1.6
      END
    END
  END
END
"#;
        let skel = translate(src);
        assert_eq!(skel.layers.len(), 1);
        let layer = &skel.layers[0];
        assert_eq!(layer.name, "roads");
        assert_eq!(layer.geom_kind.as_deref(), Some("line"));
        assert_eq!(layer.sources.len(), 1);
        assert_eq!(layer.sources[0].source_table(), "roads_table");
        assert_eq!(layer.sources[0].geometry_column, "geometri");
        assert_eq!(layer.classes.len(), 1);
        assert_eq!(layer.classes[0].name, "main");
        assert_eq!(layer.classes[0].when.as_deref(), Some("type = 'main'"));
        assert!(!skel.styles.is_empty());
    }

    #[test]
    fn translate_expands_scaletoken() {
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "buildings"
    TYPE POLYGON
    DATA "geometri FROM buildings_table"
    SCALETOKEN
      NAME "scale"
      VALUES
        "0" "buildings_0"
        "1000" "buildings_1"
      END
    END
    CLASS
      NAME "default"
      EXPRESSION ("1" = "1")
      STYLE
        COLOR 250 250 250
        OUTLINECOLOR 180 180 180
        WIDTH 0.6
      END
    END
  END
END
"#;
        let skel = translate(src);
        assert_eq!(skel.layers.len(), 1);
        let layer = &skel.layers[0];
        assert_eq!(layer.sources.len(), 2);
        assert_eq!(layer.sources[0].source_table(), "buildings_0");
        assert_eq!(layer.sources[0].max_denom_exclusive, Some(1000));
        assert_eq!(layer.sources[1].source_table(), "buildings_1");
        assert_eq!(layer.sources[1].max_denom_exclusive, None);
    }

    #[test]
    fn translate_skips_raster_layer_with_warning() {
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "ortho"
    TYPE RASTER
    DATA "ortho.tif"
  END
  LAYER
    NAME "roads"
    TYPE LINE
  END
END
"#;
        let skel = translate(src);
        assert_eq!(skel.layers.len(), 1);
        assert!(
            skel.layers.iter().all(|l| l.name != "ortho"),
            "RASTER layer should be skipped"
        );
        let roads = skel.layers.iter().find(|l| l.name == "roads").expect("roads layer");
        assert_eq!(roads.geom_kind.as_deref(), Some("line"));
    }

    #[test]
    fn translate_still_skips_query_layer() {
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "phantom"
    TYPE QUERY
  END
  LAYER
    NAME "roads"
    TYPE LINE
  END
END
"#;
        let skel = translate(src);
        assert_eq!(skel.layers.len(), 1);
        assert_eq!(skel.layers[0].name, "roads");
    }

    #[test]
    fn unsupported_construct_does_not_break_translation() {
        let src = r#"
MAP
  NAME "x"
  SYMBOL
    NAME "dot"
    TYPE ELLIPSE
    POINTS 1 1 END
    FILLED TRUE
  END
  LAYER
    NAME "l1"
  END
END
"#;
        let skel = translate(src);
        assert_eq!(skel.service_name.as_deref(), Some("x"));
        assert_eq!(skel.layers.len(), 1);
        assert_eq!(skel.layers[0].name, "l1");
    }

    #[test]
    fn lift_inline_subquery_extracts_table_and_where() {
        match lift_inline_subquery("(SELECT * FROM simplified.streams WHERE midtebredde IN ('12-', '2.5-12'))") {
            LiftedBinding::Table { table, filter } => {
                assert_eq!(table, "simplified.streams");
                let f = filter.expect("filter lifted");
                assert!(f.contains("midtebredde"));
                assert!(f.contains("12-"));
            }
            other => panic!("expected table binding, got {other:?}"),
        }
    }

    #[test]
    fn lift_inline_subquery_passes_through_bare_table() {
        match lift_inline_subquery("simplified.streams") {
            LiftedBinding::Table { table, filter } => {
                assert_eq!(table, "simplified.streams");
                assert!(filter.is_none());
            }
            other => panic!("expected table binding, got {other:?}"),
        }
    }

    #[test]
    fn lift_inline_subquery_emits_sql_for_join() {
        let raw = "(SELECT * FROM a JOIN b ON a.id = b.id WHERE x = 1)";
        match lift_inline_subquery(raw) {
            LiftedBinding::Sql { sql } => assert_eq!(sql, raw),
            other => panic!("expected sql binding, got {other:?}"),
        }
    }

    #[test]
    fn lift_inline_subquery_emits_sql_for_subselect_in_from() {
        let raw = "(SELECT a.id, a.geom FROM (SELECT id, geom FROM t WHERE z > 0) AS a WHERE a.id > 0)";
        match lift_inline_subquery(raw) {
            LiftedBinding::Sql { sql } => assert_eq!(sql, raw),
            other => panic!("expected sql binding, got {other:?}"),
        }
    }

    #[test]
    fn normalize_n_plus_one_handles_round_bases() {
        assert_eq!(normalize_n_plus_one(0), 0);
        assert_eq!(normalize_n_plus_one(1), 1);
        assert_eq!(normalize_n_plus_one(101), 100);
        assert_eq!(normalize_n_plus_one(2_501), 2_500);
        assert_eq!(normalize_n_plus_one(25_001), 25_000);
        assert_eq!(normalize_n_plus_one(100_001), 100_000);
        // not on a round base - left alone.
        assert_eq!(normalize_n_plus_one(2_502), 2_502);
        assert_eq!(normalize_n_plus_one(123), 123);
    }

    #[test]
    fn parse_scale_token_normalizes_n_plus_one() {
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "buildings"
    TYPE POLYGON
    DATA "geometri FROM buildings_table"
    SCALETOKEN
      NAME "scale"
      VALUES
        "0" "buildings_0"
        "25001" "buildings_1"
      END
    END
  END
END
"#;
        let skel = translate(src);
        let layer = &skel.layers[0];
        assert_eq!(layer.sources[0].max_denom_exclusive, Some(25_000));
    }

    #[test]
    fn translate_symbol_circle_then_class_emits_marker() {
        let src = r#"
MAP
  NAME "demo"
  SYMBOL
    NAME "circle"
    TYPE ELLIPSE
    POINTS 1 1 END
    FILLED TRUE
  END
  LAYER
    NAME "stops"
    TYPE POINT
    DATA "geom FROM stops"
    CLASS
      NAME "default"
      STYLE
        SYMBOL "circle"
        SIZE 8
        COLOR 30 30 30
      END
    END
  END
END
"#;
        let skel = translate(src);
        assert!(skel.symbols.contains_key("circle"));
        // the style emitted for `stops` class default should carry a marker.
        let style = skel
            .styles
            .iter()
            .find(|s| s.name.starts_with("point_stops_"))
            .expect("point style emitted");
        let m = style.marker.as_ref().expect("marker resolved from SYMBOL");
        match m {
            crate::emitter::EmitMarker::Builtin { kind, size, .. } => {
                assert_eq!(*kind, MarkerKind::Circle);
                assert!((size - 8.0).abs() < f32::EPSILON);
            }
            other => panic!("expected builtin marker, got {other:?}"),
        }
    }

    #[test]
    fn translate_symbol_hatch_then_class_emits_fill_kind_hatch() {
        let src = r#"
MAP
  NAME "demo"
  SYMBOL
    NAME "lines"
    TYPE HATCH
    ANGLE 45
    SIZE 4
  END
  LAYER
    NAME "wetlands"
    TYPE POLYGON
    DATA "geom FROM wetlands"
    CLASS
      NAME "default"
      STYLE
        SYMBOL "lines"
        WIDTH 0.5
        COLOR 100 110 120
      END
    END
  END
END
"#;
        let skel = translate(src);
        let def = skel.symbols.get("lines").expect("hatch symbol parsed");
        assert!(matches!(def, crate::emitter::SymbolDef::Hatch { .. }));
        let style = skel
            .styles
            .iter()
            .find(|s| s.name.starts_with("poly_wetlands_"))
            .expect("polygon style emitted");
        match &style.fill {
            Some(crate::emitter::EmitFill::Hatch {
                spacing,
                angle_deg,
                line_width,
                colour,
            }) => {
                assert!((spacing - 4.0).abs() < f32::EPSILON);
                assert!((angle_deg - 45.0).abs() < f32::EPSILON);
                assert!((line_width - 0.5).abs() < f32::EPSILON);
                assert_eq!(*colour, mars_style::Colour::rgb(100, 110, 120));
            }
            other => panic!("expected hatch fill, got {other:?}"),
        }
    }

    #[test]
    fn translate_unknown_symbol_reference_warns_and_no_marker_emitted() {
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "x"
    TYPE POINT
    DATA "geom FROM t"
    CLASS
      NAME "default"
      STYLE
        SYMBOL "missing"
        SIZE 6
      END
    END
  END
END
"#;
        let skel = translate(src);
        let style = skel
            .styles
            .iter()
            .find(|s| s.name.starts_with("point_x_"))
            .expect("style emitted");
        // unknown SYMBOL reference: marker stays None, fill stays None
        // (the STYLE block had no COLOR). config-validation will accept
        // this as a no-op style; operator can hand-edit.
        assert!(style.marker.is_none());
        assert!(style.fill.is_none());
    }

    #[test]
    fn classitem_expands_named_classes_into_implicit_predicates() {
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "roads"
    TYPE LINE
    DATA "geom FROM r"
    CLASSITEM "type"
    CLASS
      NAME "main"
      STYLE
        COLOR 0 0 0
      END
    END
    CLASS
      NAME "side"
      STYLE
        COLOR 200 200 200
      END
    END
  END
END
"#;
        let skel = translate(src);
        let layer = &skel.layers[0];
        assert_eq!(layer.classes[0].when.as_deref(), Some("type = 'main'"));
        assert_eq!(layer.classes[1].when.as_deref(), Some("type = 'side'"));
    }

    #[test]
    fn labelitem_fills_text_when_label_has_no_text() {
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "places"
    TYPE POINT
    DATA "geom FROM p"
    LABELITEM "name"
    LABEL
      FONT "Sans"
      SIZE 10
      COLOR 0 0 0
    END
  END
END
"#;
        let skel = translate(src);
        let layer = &skel.layers[0];
        let lbl = layer.label.as_ref().expect("label emitted");
        assert_eq!(lbl.text, "{name}");
    }

    #[test]
    fn label_angle_follow_sets_line_placement() {
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "roads"
    TYPE LINE
    DATA "geom FROM r"
    LABEL
      TEXT "{name}"
      ANGLE FOLLOW
      REPEATDISTANCE 250
    END
  END
END
"#;
        let skel = translate(src);
        let lbl = skel.layers[0].label.as_ref().expect("label emitted");
        let p = lbl.placement_line.expect("line placement");
        assert!((p.repeat_m.unwrap() - 250.0).abs() < 1e-3);
    }

    #[test]
    fn symbol_pixmap_type_translates_to_image_fill() {
        let src = r#"
MAP
  NAME "demo"
  SYMBOL
    NAME "brick"
    TYPE PIXMAP
    IMAGE "/abs/path/brick.png"
  END
  LAYER
    NAME "walls"
    TYPE POLYGON
    DATA "geom FROM w"
    CLASS
      NAME "default"
      STYLE
        SYMBOL "brick"
      END
    END
  END
END
"#;
        let skel = translate(src);
        match skel.symbols.get("brick") {
            Some(crate::emitter::SymbolDef::Pixmap { source_image }) => {
                assert_eq!(source_image.as_deref(), Some("/abs/path/brick.png"));
            }
            other => panic!("expected SymbolDef::Pixmap, got {other:?}"),
        }
        let style = skel
            .styles
            .iter()
            .find(|s| s.name.starts_with("poly_walls_"))
            .expect("polygon style emitted");
        match &style.fill {
            Some(crate::emitter::EmitFill::Image { name }) => assert_eq!(name, "brick"),
            other => panic!("expected EmitFill::Image, got {other:?}"),
        }
    }

    #[test]
    fn symbol_truly_unknown_type_lands_as_typed_not_implemented() {
        let src = r#"
MAP
  NAME "demo"
  SYMBOL
    NAME "weird"
    TYPE CARTOLINE
  END
  LAYER
    NAME "stations"
    TYPE POINT
    DATA "geom FROM s"
    CLASS
      NAME "default"
      STYLE
        SYMBOL "weird"
        SIZE 8
      END
    END
  END
END
"#;
        let skel = translate(src);
        match skel.symbols.get("weird") {
            Some(crate::emitter::SymbolDef::NotImplemented { raw_type }) => {
                assert_eq!(raw_type, "CARTOLINE");
            }
            other => panic!("expected NotImplemented variant, got {other:?}"),
        }
    }

    #[test]
    fn symbol_truetype_resolves_to_glyph_marker() {
        let src = r#"
MAP
  NAME "demo"
  SYMBOL
    NAME "letter_t"
    TYPE TRUETYPE
    FONT "sans"
    CHARACTER "T"
  END
  LAYER
    NAME "stations"
    TYPE POINT
    DATA "geom FROM s"
    CLASS
      NAME "default"
      STYLE
        SYMBOL "letter_t"
        SIZE 14
      END
    END
  END
END
"#;
        let skel = translate(src);
        let style = skel
            .styles
            .iter()
            .find(|s| s.name.starts_with("point_stations_"))
            .expect("point style emitted");
        let m = style.marker.as_ref().expect("glyph marker");
        match m {
            crate::emitter::EmitMarker::Glyph {
                font_family,
                character,
                size,
                ..
            } => {
                assert_eq!(font_family, "sans");
                assert_eq!(character, "T");
                assert!((size - 14.0).abs() < f32::EPSILON);
            }
            other => panic!("expected glyph marker, got {other:?}"),
        }
    }

    #[test]
    fn multi_style_class_emits_passes_in_declared_order() {
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "boundaries"
    TYPE POLYGON
    DATA "geom FROM b"
    CLASS
      NAME "default"
      STYLE
        COLOR 240 240 230
      END
      STYLE
        OUTLINECOLOR 40 40 60
        WIDTH 4
      END
      STYLE
        OUTLINECOLOR 220 220 240
        WIDTH 1.5
      END
    END
  END
END
"#;
        let skel = translate(src);
        let layer = &skel.layers[0];
        let cls = &layer.classes[0];
        match &cls.style {
            crate::emitter::ClassStyleAttach::Passes(passes) => {
                assert_eq!(passes.len(), 3, "three STYLE blocks should yield three passes");
                // pass 0: solid fill, no stroke
                assert!(matches!(passes[0].fill, Some(crate::emitter::EmitFill::Hex(_))));
                assert!(passes[0].stroke.is_none());
                // pass 1: thick dark stroke, no fill
                assert!(passes[1].fill.is_none());
                assert_eq!(passes[1].stroke, Some(mars_style::Colour::rgb(40, 40, 60)));
                assert_eq!(passes[1].stroke_width, Some(4.0));
                // pass 2: thin light stroke, no fill
                assert!(passes[2].fill.is_none());
                assert_eq!(passes[2].stroke, Some(mars_style::Colour::rgb(220, 220, 240)));
                assert_eq!(passes[2].stroke_width, Some(1.5));
            }
            other => panic!("expected ClassStyleAttach::Passes, got {other:?}"),
        }
        // multi-pass classes do not register named entries in the styles
        // registry; this layer's class should not appear by its style_name.
        assert!(skel.styles.iter().all(|s| !s.name.starts_with("poly_boundaries_")));
    }

    #[test]
    fn single_style_class_still_emits_ref_attachment() {
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "x"
    TYPE LINE
    DATA "geom FROM x"
    CLASS
      NAME "default"
      STYLE
        COLOR 0 0 0
        WIDTH 1
      END
    END
  END
END
"#;
        let skel = translate(src);
        let cls = &skel.layers[0].classes[0];
        match &cls.style {
            crate::emitter::ClassStyleAttach::Ref(name) => {
                assert!(skel.styles.iter().any(|s| &s.name == name));
            }
            other => panic!("expected ClassStyleAttach::Ref, got {other:?}"),
        }
    }

    #[test]
    fn symbol_vector_with_points_resolves_to_vector_shape() {
        let src = r#"
MAP
  NAME "demo"
  SYMBOL
    NAME "trekant"
    TYPE VECTOR
    FILLED TRUE
    POINTS
      0 0
      1 0
      0.5 1
    END
  END
  LAYER
    NAME "x"
    TYPE POINT
    DATA "geom FROM t"
    CLASS
      NAME "default"
      STYLE
        SYMBOL "trekant"
        SIZE 8
      END
    END
  END
END
"#;
        let skel = translate(src);
        let style = skel
            .styles
            .iter()
            .find(|s| s.name.starts_with("point_x_"))
            .expect("point style emitted");
        let m = style.marker.as_ref().expect("vector marker");
        match m {
            crate::emitter::EmitMarker::Vector {
                points, filled, size, ..
            } => {
                assert_eq!(points.len(), 3);
                assert!(*filled);
                assert!((size - 8.0).abs() < f32::EPSILON);
            }
            other => panic!("expected vector marker, got {other:?}"),
        }
    }

    #[test]
    fn complex_data_inline_select_falls_back_to_sql_binding() {
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "joined"
    TYPE LINE
    DATA "geom FROM (SELECT a.geom FROM a JOIN b USING (id) WHERE x = 1) AS r"
    CLASS
      NAME "default"
      STYLE
        COLOR 0 0 0
      END
    END
  END
END
"#;
        let skel = translate(src);
        let layer = &skel.layers[0];
        assert_eq!(layer.sources.len(), 1);
        match &layer.sources[0].source {
            crate::emitter::BindingSource::Sql(_) => {}
            other => panic!("expected sql binding, got {other:?}"),
        }
    }

    #[test]
    fn map_resolution_lifts_to_service_scale_dpi() {
        let src = r#"
MAP
  NAME "demo"
  RESOLUTION 96
  LAYER
    NAME "x"
    TYPE LINE
  END
END
"#;
        let skel = translate(src);
        assert_eq!(skel.scale_dpi, Some(96.0));
        let yaml = crate::emitter::render(&skel, &crate::emitter::default_bands());
        assert!(
            yaml.contains("scale_dpi: 96"),
            "rendered yaml missing scale_dpi field; got:\n{yaml}"
        );
    }

    #[test]
    fn map_maxsize_lifts_to_wms_max_image_dimension() {
        let src = r#"
MAP
  NAME "demo"
  MAXSIZE 8192
  LAYER
    NAME "x"
    TYPE LINE
  END
END
"#;
        let skel = translate(src);
        assert_eq!(skel.wms_max_image_dimension, Some(8192));
        let yaml = crate::emitter::render(&skel, &crate::emitter::default_bands());
        assert!(
            yaml.contains("max_image_dimension: 8192"),
            "rendered yaml missing max_image_dimension field; got:\n{yaml}"
        );
    }

    #[test]
    fn composite_opacity_lowers_to_style_opacity_multiplier() {
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "roads"
    TYPE LINE
    DATA "geom FROM r"
    COMPOSITE
      OPACITY 50
    END
    CLASS
      NAME "default"
      STYLE
        COLOR 0 0 0
        WIDTH 1.0
      END
    END
  END
END
"#;
        let skel = translate(src);
        let style = skel
            .styles
            .iter()
            .find(|s| s.name.starts_with("line_roads_"))
            .expect("line style emitted");
        let op = style.opacity.expect("opacity set from COMPOSITE OPACITY");
        assert!((op - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn composite_opacity_composes_with_style_opacity() {
        // mapfile permits both COMPOSITE.OPACITY (layer-wide) and STYLE.OPACITY
        // (per-pass) - mars-style composes multiplicatively at draw time, so
        // we pre-compose at translate time too.
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "x"
    TYPE LINE
    DATA "geom FROM t"
    COMPOSITE
      OPACITY 50
    END
    CLASS
      NAME "default"
      STYLE
        COLOR 0 0 0
        OPACITY 40
      END
    END
  END
END
"#;
        let skel = translate(src);
        let style = skel
            .styles
            .iter()
            .find(|s| s.name.starts_with("line_x_"))
            .expect("line style emitted");
        let op = style.opacity.expect("opacity set");
        // 0.5 * 0.4 = 0.2
        assert!((op - 0.2).abs() < 1e-5);
    }

    #[test]
    fn comments_and_case_are_handled() {
        let src = r#"
map # top-level
  name "abc"   # service name
  layer
    name "only"
  end
end
"#;
        let skel = translate(src);
        assert_eq!(skel.service_name.as_deref(), Some("abc"));
        assert_eq!(skel.layers.len(), 1);
        assert_eq!(skel.layers[0].name, "only");
    }

    #[test]
    fn apply_font_aliases_rewrites_label_and_symbol_references() {
        let src = r#"
MAP
  NAME "demo"
  SYMBOL
    NAME "letter_t"
    TYPE TRUETYPE
    FONT "sans"
    CHARACTER "T"
  END
  LAYER
    NAME "places"
    TYPE POINT
    DATA "geom FROM p"
    LABEL
      TEXT "{name}"
      FONT "sans"
      SIZE 10
      COLOR 0 0 0
    END
    CLASS
      NAME "default"
      STYLE
        SYMBOL "letter_t"
        SIZE 12
      END
    END
  END
END
"#;
        let tokens = scan(src);
        let mut skel = translate_tokens(&tokens, None, None, false);
        // sanity: pre-rewrite the family is still the alias.
        let label_style = skel
            .styles
            .iter()
            .find(|s| s.style_type == "label")
            .expect("label style emitted");
        assert_eq!(label_style.font_family.as_deref(), Some("sans"));

        let aliases = fontset::from_pairs([("sans", "DejaVu Sans")]);
        apply_font_aliases(&mut skel, &aliases);

        let label_style = skel
            .styles
            .iter()
            .find(|s| s.style_type == "label")
            .expect("label style emitted");
        assert_eq!(label_style.font_family.as_deref(), Some("DejaVu Sans"));

        // glyph marker on the point style: alias also rewritten.
        let point_style = skel
            .styles
            .iter()
            .find(|s| s.name.starts_with("point_places_"))
            .expect("point style emitted");
        match point_style.marker.as_ref().expect("glyph marker") {
            crate::emitter::EmitMarker::Glyph { font_family, .. } => {
                assert_eq!(font_family, "DejaVu Sans");
            }
            other => panic!("expected glyph marker, got {other:?}"),
        }

        // symbol-table entry mirrors the style marker rewrite.
        match skel.symbols.get("letter_t").expect("symbol kept") {
            crate::emitter::SymbolDef::Glyph { font_family, .. } => {
                assert_eq!(font_family, "DejaVu Sans");
            }
            other => panic!("expected glyph symbol def, got {other:?}"),
        }
    }

    #[test]
    fn apply_font_aliases_passes_through_unknown_aliases() {
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "places"
    TYPE POINT
    DATA "geom FROM p"
    CLASS
      NAME "default"
      LABEL
        TEXT "{name}"
        FONT "mystery"
        SIZE 10
        COLOR 0 0 0
      END
    END
  END
END
"#;
        let tokens = scan(src);
        let mut skel = translate_tokens(&tokens, None, None, false);
        let aliases = fontset::from_pairs([("sans", "DejaVu Sans")]);
        apply_font_aliases(&mut skel, &aliases);
        let label_style = skel
            .styles
            .iter()
            .find(|s| s.style_type == "label")
            .expect("label style emitted");
        assert_eq!(label_style.font_family.as_deref(), Some("mystery"));
    }

    #[test]
    fn fontset_directive_loaded_from_disk_resolves_alias() {
        // build a tmp mapfile that points at a fontset.txt next to it; the
        // fontset references the bundled DejaVu Sans from mars-text so the
        // resolution path is deterministic across hosts.
        let tmp = std::env::temp_dir().join("mars_import_fontset_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let font_src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/support/mars-text/test_fonts/DejaVuSans.ttf");
        if !font_src.exists() {
            // sibling crate not available in this build matrix; skip.
            return;
        }
        let font_dst = tmp.join("DejaVuSans.ttf");
        std::fs::copy(&font_src, &font_dst).unwrap();
        let fontset_path = tmp.join("fonts.list");
        std::fs::write(&fontset_path, "sans DejaVuSans.ttf\n").unwrap();
        let map_path = tmp.join("demo.map");
        std::fs::write(
            &map_path,
            r#"MAP
  NAME "demo"
  FONTSET "fonts.list"
  LAYER
    NAME "places"
    TYPE POINT
    DATA "geom FROM p"
    CLASS
      NAME "default"
      LABEL
        TEXT "{name}"
        FONT "sans"
        SIZE 10
        COLOR 0 0 0
      END
    END
  END
END
"#,
        )
        .unwrap();

        let tokens = crate::scanner::scan_file(&map_path).unwrap();
        let skel = translate_tokens(&tokens, None, map_path.parent(), false);
        let label_style = skel
            .styles
            .iter()
            .find(|s| s.style_type == "label")
            .expect("label style emitted");
        let family = label_style.font_family.as_deref().unwrap();
        assert!(
            family.to_ascii_lowercase().contains("dejavu"),
            "expected dejavu family from alias rewrite, got {family:?}",
        );
    }
}

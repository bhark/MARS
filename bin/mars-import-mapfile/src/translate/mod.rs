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
mod label;
mod layer;
mod resolved;
mod style_block;
mod symbol;

use std::collections::HashSet;

use tracing::warn;

use crate::directive::MapDirective;
use crate::emitter::Skeleton;
#[cfg(test)]
use crate::scanner::scan;
use crate::scanner::{Token, block_range, is_block_opener};

use self::emit::emit_symbol;
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
    "FONTSET",
    "LEGEND",
    "PROJECTION",
    "OUTPUTFORMAT",
    "FEATURE",
    "JOIN",
    "COMPOSITE",
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
    translate_tokens(&tokens, None)
}

pub(crate) fn translate_tokens(tokens: &[Token], include_layers: Option<&HashSet<String>>) -> Skeleton {
    let mut skel = Skeleton::default();

    let map_slice: &[Token] = match tokens
        .iter()
        .position(|t| t.keyword.eq_ignore_ascii_case("MAP"))
        .and_then(|i| block_range(tokens, i))
    {
        Some(r) => &tokens[r.start + 1..r.end.saturating_sub(1).max(r.start + 1)],
        None => tokens,
    };

    walk(map_slice, &mut skel, include_layers);
    skel
}

fn walk(tokens: &[Token], skel: &mut Skeleton, include_layers: Option<&HashSet<String>>) {
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
                handle_layer(body, open.line, skel, include_layers);
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
    fn translate_emits_raster_layer_as_kind_raster_scaffold() {
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
        assert_eq!(skel.layers.len(), 2);
        let ortho = skel.layers.iter().find(|l| l.name == "ortho").expect("ortho layer");
        assert_eq!(ortho.geom_kind.as_deref(), Some("raster"));
        assert!(ortho.sources.is_empty(), "raster scaffold has no vector sources");
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
            crate::emitter::EmitMarker::Builtin { kind, size } => {
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
            } => {
                assert_eq!(font_family, "sans");
                assert_eq!(character, "T");
                assert!((size - 14.0).abs() < f32::EPSILON);
            }
            other => panic!("expected glyph marker, got {other:?}"),
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
}

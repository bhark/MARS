//! mapfile-to-skeleton translation pipeline.

use std::collections::{BTreeSet, HashSet};

use tracing::warn;

use crate::emitter::{ClassSkeleton, LabelSkeleton, LayerSkeleton, Skeleton, SourceSkeleton};
#[cfg(test)]
use crate::scanner::scan;
use crate::scanner::{Token, block_range, is_block_opener};
use crate::style::{parse_class, parse_label};

/// keywords whose presence we don't translate yet. some are block openers,
/// some are scalar directives - `walk` handles both.
const UNSUPPORTED: &[&str] = &[
    "SYMBOL",
    "FONTSET",
    "LEGEND",
    "PROJECTION",
    "METADATA",
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
        let kw = t.keyword.to_ascii_uppercase();

        if kw == "NAME" && skel.service_name.is_none() {
            if let Some(v) = t.args.first() {
                skel.service_name = Some(v.clone());
            }
            i += 1;
            continue;
        }
        if kw == "TITLE" && skel.service_title.is_none() {
            if let Some(v) = t.args.first() {
                skel.service_title = Some(v.clone());
            }
            i += 1;
            continue;
        }

        if kw == "LAYER" {
            let range = block_range(tokens, i).unwrap_or(i..i + 1);
            let body: &[Token] = if range.end > range.start + 1 {
                &tokens[range.start + 1..range.end - 1]
            } else {
                &[]
            };
            handle_layer(body, t.line, skel, include_layers);
            i = range.end;
            continue;
        }

        if is_unsupported(&kw) {
            warn!(line = t.line, keyword = %kw, "unsupported mapfile construct");
            if is_block_opener(&kw)
                && let Some(r) = block_range(tokens, i)
            {
                i = r.end;
                continue;
            }
        }
        i += 1;
    }
}

fn handle_layer(body: &[Token], layer_line: usize, skel: &mut Skeleton, include_layers: Option<&HashSet<String>>) {
    let mut name: Option<String> = None;
    let mut title: Option<String> = None;
    let mut layer_type: Option<String> = None;
    let mut data: Option<String> = None;
    let mut _min_scale_denom: Option<u64> = None;
    let mut max_scale_denom: Option<u64> = None;
    let mut scale_token_values: Vec<(u64, String)> = Vec::new();
    let mut processing_items: Option<String> = None;
    let mut classes: Vec<ClassSkeleton> = Vec::new();
    let mut label: Option<LabelSkeleton> = None;

    // peek name first for filtering
    for t in body {
        if t.keyword.eq_ignore_ascii_case("NAME") {
            if let Some(n) = t.args.first() {
                name = Some(n.clone());
            }
            break;
        }
    }

    if let Some(set) = include_layers {
        let keep = name.as_ref().is_some_and(|n| set.contains(&n.to_lowercase()));
        if !keep {
            return;
        }
    }

    let mut i = 0;
    while i < body.len() {
        let t = &body[i];
        let kw = t.keyword.to_ascii_uppercase();
        match kw.as_str() {
            "NAME" if name.is_none() => {
                name = t.args.first().cloned();
                i += 1;
                continue;
            }
            "TITLE" if title.is_none() => {
                title = t.args.first().cloned();
                i += 1;
                continue;
            }
            "TYPE" if layer_type.is_none() => {
                layer_type = t.args.first().cloned();
                i += 1;
                continue;
            }
            "DATA" if data.is_none() => {
                data = Some(t.args.join(" "));
                i += 1;
                continue;
            }
            "MINSCALEDENOM" | "MAXSCALEDENOM" => {
                if let Some(arg) = t.args.first() {
                    match arg.parse::<f64>() {
                        Ok(v) if v.is_finite() && v >= 0.0 => {
                            let n = normalize_n_plus_one(v as u64);
                            if kw == "MINSCALEDENOM" {
                                _min_scale_denom = Some(n);
                            } else {
                                max_scale_denom = Some(n);
                            }
                        }
                        _ => warn!(line = t.line, keyword = %kw, value = %arg, "could not parse scale denom"),
                    }
                }
                i += 1;
                continue;
            }
            "PROCESSING" => {
                if let Some(arg) = t.args.first() {
                    let up = arg.to_ascii_uppercase();
                    if let Some(rest) = up.strip_prefix("ITEMS=") {
                        processing_items = Some(rest.to_string());
                    }
                }
                i += 1;
                continue;
            }
            "SCALETOKEN" => {
                if let Some(r) = block_range(body, i) {
                    let st_body = &body[r.start + 1..r.end - 1];
                    let mut j = 0;
                    while j < st_body.len() {
                        let st_t = &st_body[j];
                        if st_t.keyword.eq_ignore_ascii_case("VALUES")
                            && let Some(vr) = block_range(st_body, j)
                        {
                            scale_token_values = parse_scale_token(&st_body[vr.start + 1..vr.end - 1]);
                            j = vr.end;
                            continue;
                        }
                        j += 1;
                    }
                    i = r.end;
                    continue;
                }
            }
            "CLASS" => {
                if let Some(r) = block_range(body, i) {
                    let resolved_name = name.clone().unwrap_or_else(|| format!("unnamed_layer_l{layer_line}"));
                    let geom = layer_type.as_deref().and_then(mapfile_type_to_geom);
                    if let Some(cls) = parse_class(
                        &body[r.start + 1..r.end - 1],
                        t.line,
                        &resolved_name,
                        geom.unwrap_or("polygon"),
                        skel,
                    ) {
                        classes.push(cls);
                    }
                    i = r.end;
                    continue;
                }
            }
            "LABEL" => {
                if let Some(r) = block_range(body, i) {
                    let resolved_name = name.clone().unwrap_or_else(|| format!("unnamed_layer_l{layer_line}"));
                    if let Some(lbl) = parse_label(&body[r.start + 1..r.end - 1], t.line, &resolved_name, skel) {
                        label = Some(lbl);
                    }
                    i = r.end;
                    continue;
                }
            }
            _ => {}
        }

        if is_unsupported(&kw) {
            warn!(line = t.line, keyword = %kw, "unsupported mapfile construct");
            if is_block_opener(&kw)
                && let Some(r) = block_range(body, i)
            {
                i = r.end;
                continue;
            }
        }
        i += 1;
    }

    let resolved_name = name.unwrap_or_else(|| format!("unnamed_layer_l{layer_line}"));

    if let Some(ref t) = layer_type {
        let up = t.to_ascii_uppercase();
        if up == "RASTER" || up == "QUERY" {
            warn!(line = layer_line, layer = %resolved_name, "skipping RASTER/QUERY layer");
            return;
        }
    }

    let geom_kind = layer_type
        .as_ref()
        .and_then(|t| mapfile_type_to_geom(t).map(|s| s.to_string()));

    let (geometry_column, from_table) = parse_data(data.as_deref());

    let mut sources = Vec::new();
    if !scale_token_values.is_empty() {
        let gc = geometry_column.clone().unwrap_or_else(|| "geometri".into());
        let id_col = processing_items.as_deref().and_then(guess_id_column);
        let n = scale_token_values.len();
        for (idx, (_min_denom, table)) in scale_token_values.iter().enumerate() {
            let max_denom = if idx + 1 < n {
                Some(scale_token_values[idx + 1].0)
            } else {
                max_scale_denom
            };
            let (real_table, filter) = lift_inline_subquery(table);
            sources.push(SourceSkeleton {
                max_denom_exclusive: max_denom,
                from: real_table,
                filter,
                geometry_column: gc.clone(),
                id_column: id_col.clone(),
                attributes: Vec::new(),
            });
        }
    } else if let Some(table) = from_table {
        let (real_table, filter) = lift_inline_subquery(&table);
        sources.push(SourceSkeleton {
            max_denom_exclusive: max_scale_denom,
            from: real_table,
            filter,
            geometry_column: geometry_column.unwrap_or_else(|| "geometri".into()),
            id_column: processing_items.as_deref().and_then(guess_id_column),
            attributes: Vec::new(),
        });
    }

    // collect attributes from class expressions and any per-tier filter idents
    // (config validation requires every filter ident to be declared on the
    // binding's attributes ∪ id_column).
    let mut all_attrs = BTreeSet::new();
    for cls in &classes {
        if let Some(ref when) = cls.when
            && let Ok(expr) = mars_expr::parse(when)
        {
            mars_expr::collect_idents(&expr, &mut all_attrs);
        }
    }
    for src in &sources {
        if let Some(ref f) = src.filter
            && let Ok(expr) = mars_expr::parse(f)
        {
            mars_expr::collect_idents(&expr, &mut all_attrs);
        }
    }
    let attrs_vec: Vec<String> = all_attrs.into_iter().collect();
    for src in &mut sources {
        src.attributes = attrs_vec.clone();
    }

    skel.layers.push(LayerSkeleton {
        name: resolved_name,
        title,
        geom_kind,
        sources,
        classes,
        label,
    });
}

pub(crate) fn mapfile_type_to_geom(t: &str) -> Option<&str> {
    match t.to_ascii_uppercase().as_str() {
        "POINT" => Some("point"),
        "LINE" | "POLYLINE" => Some("line"),
        "POLYGON" => Some("polygon"),
        _ => None,
    }
}

/// strip a trailing ` USING ...` clause from a mapfile DATA / SCALETOKEN value.
fn strip_using(s: &str) -> &str {
    let upper = s.to_ascii_uppercase();
    if let Some(pos) = upper.find(" USING ") {
        &s[..pos]
    } else {
        s
    }
}

/// Detect a mapfile inline DATA subquery of the shape
/// `( SELECT ... FROM <table> WHERE <expr> ) [AS alias]` and split it into the
/// real table and the WHERE clause. Anything that doesn't fit this exact
/// shape (joins, sub-selects, bare table refs) falls through with the input
/// returned as-is and no filter. Heuristic-only - operators are expected to
/// hand-edit the YAML for anything more elaborate.
pub(crate) fn lift_inline_subquery(raw: &str) -> (String, Option<String>) {
    let trimmed = raw.trim();
    let inner = match trimmed.strip_prefix('(') {
        Some(rest) => match rest.rsplit_once(')') {
            // tolerate trailing ` AS alias` after the closing paren.
            Some((body, _tail)) => body.trim(),
            None => return (raw.to_string(), None),
        },
        None => return (raw.to_string(), None),
    };
    let upper = inner.to_ascii_uppercase();
    if !upper.trim_start().starts_with("SELECT") {
        return (raw.to_string(), None);
    }
    let from_pos = match upper.find(" FROM ") {
        Some(p) => p + " FROM ".len(),
        None => return (raw.to_string(), None),
    };
    let where_pos = match upper[from_pos..].find(" WHERE ") {
        Some(p) => from_pos + p,
        None => return (raw.to_string(), None),
    };
    let table = inner[from_pos..where_pos].trim().to_string();
    // accept simple `schema.table` / `table` / `"table"`; bail on anything with
    // joins, commas, or sub-selects so we don't fabricate a fragile from:.
    if table.contains(',') || table.contains(' ') || table.contains('(') {
        return (raw.to_string(), None);
    }
    let where_clause = inner[where_pos + " WHERE ".len()..].trim();
    let cleaned_table = table.trim_matches('"').to_string();
    if where_clause.is_empty() {
        return (cleaned_table, None);
    }
    // round-trip through the mars-expr parser when feasible to normalise
    // quoting/spacing. mapfile WHERE is SQL; round-trip failure means the
    // expression is outside the DSL, so emit the raw text and let config
    // validation reject it loudly rather than silently dropping.
    let normalised = mars_expr::parse(where_clause)
        .map(|e| e.to_string())
        .unwrap_or_else(|_| where_clause.to_string());
    (cleaned_table, Some(normalised))
}

fn parse_data(data: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(d) = data else { return (None, None) };
    let cleaned = strip_using(d);
    let cleaned = cleaned.trim().trim_matches('"');
    let cleaned_upper = cleaned.to_ascii_uppercase();
    if let Some(pos) = cleaned_upper.find(" FROM ") {
        let geom = cleaned[..pos].trim().to_string();
        let table = cleaned[pos + 6..].trim().to_string();
        (Some(geom), Some(table))
    } else {
        (None, Some(cleaned.to_string()))
    }
}

fn guess_id_column(items: &str) -> Option<String> {
    let parts: Vec<&str> = items.split(',').map(|s| s.trim()).collect();
    parts
        .iter()
        .find(|s| s.eq_ignore_ascii_case("ogc_fid"))
        .copied()
        .or_else(|| parts.iter().find(|s| s.eq_ignore_ascii_case("id")).copied())
        .or_else(|| parts.iter().find(|s| s.to_ascii_lowercase().ends_with("_fid")).copied())
        .map(|s| s.to_string())
}

fn parse_scale_token(body: &[Token]) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    for t in body {
        if t.keyword.eq_ignore_ascii_case("END") {
            break;
        }
        let raw = match t.keyword.parse::<f64>() {
            Ok(v) if v.is_finite() && v >= 0.0 => v as u64,
            _ => continue,
        };
        let min = normalize_n_plus_one(raw);
        if let Some(table) = t.args.first() {
            let cleaned = strip_using(table).trim().trim_matches('"').to_string();
            if !cleaned.is_empty() {
                out.push((min, cleaned));
            }
        }
    }
    out
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
    use super::*;

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
        assert_eq!(layer.sources[0].from, "roads_table");
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
        assert_eq!(layer.sources[0].from, "buildings_0");
        assert_eq!(layer.sources[0].max_denom_exclusive, Some(1000));
        assert_eq!(layer.sources[1].from, "buildings_1");
        assert_eq!(layer.sources[1].max_denom_exclusive, None);
    }

    #[test]
    fn translate_skips_raster_layer() {
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "ortho"
    TYPE RASTER
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
        let (t, f) = lift_inline_subquery("(SELECT * FROM simplified.streams WHERE midtebredde IN ('12-', '2.5-12'))");
        assert_eq!(t, "simplified.streams");
        let f = f.expect("filter lifted");
        assert!(f.contains("midtebredde"));
        assert!(f.contains("12-"));
    }

    #[test]
    fn lift_inline_subquery_passes_through_bare_table() {
        let (t, f) = lift_inline_subquery("simplified.streams");
        assert_eq!(t, "simplified.streams");
        assert!(f.is_none());
    }

    #[test]
    fn lift_inline_subquery_skips_joins_and_complex_from() {
        let raw = "(SELECT * FROM a JOIN b ON a.id = b.id WHERE x = 1)";
        let (t, f) = lift_inline_subquery(raw);
        // join means we keep the raw text and emit no filter.
        assert_eq!(t, raw);
        assert!(f.is_none());
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

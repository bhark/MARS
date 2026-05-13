//! mapfile-to-skeleton translation pipeline.

use std::collections::{BTreeSet, HashSet};

use tracing::warn;

use crate::emitter::{
    BindingSource, ClassSkeleton, LabelSkeleton, LayerSkeleton, MarkerKind, Skeleton, SourceSkeleton, SymbolDef,
};
use crate::parsing;
#[cfg(test)]
use crate::scanner::scan;
use crate::scanner::{Token, block_range, is_block_opener};
use crate::style::{parse_class, parse_label};

/// keywords whose presence we don't translate yet. some are block openers,
/// some are scalar directives - `walk` handles both.
const UNSUPPORTED: &[&str] = &[
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

        if kw == "SYMBOL" {
            let range = block_range(tokens, i).unwrap_or(i..i + 1);
            let body: &[Token] = if range.end > range.start + 1 {
                &tokens[range.start + 1..range.end - 1]
            } else {
                &[]
            };
            if let Some((name, def)) = parse_symbol(body) {
                skel.symbols.insert(name, def);
            }
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
    // CLASSITEM names a column whose value drives the implicit per-class
    // `<col> = '<NAME>'` expression when a CLASS has no EXPRESSION.
    let mut class_item: Option<String> = None;
    // LABELITEM names a column whose value is the label text when LABEL
    // has no TEXT.
    let mut label_item: Option<String> = None;

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
            "CLASSITEM" if class_item.is_none() => {
                class_item = parsing::first_unquoted(t);
                i += 1;
                continue;
            }
            "LABELITEM" if label_item.is_none() => {
                label_item = parsing::first_unquoted(t);
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

    // CLASSITEM expansion: a CLASS with NAME but no EXPRESSION inherits an
    // implicit `<classitem> = '<NAME>'` predicate. mirrors mapserver's
    // attribute-keyed class semantics.
    if let Some(item) = class_item.as_deref() {
        for cls in &mut classes {
            if cls.when.is_none()
                && let Some(value) = cls.title.as_deref()
            {
                cls.when = Some(format!("{item} = '{}'", value.replace('\'', "''")));
            }
        }
    }
    // LABELITEM: if the LABEL block had no TEXT, the layer's labelitem
    // becomes a `{<col>}` template referencing the column.
    if let (Some(item), Some(lbl)) = (label_item.as_deref(), label.as_mut())
        && lbl.text.is_empty()
    {
        lbl.text = format!("{{{item}}}");
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
            let (source, filter) = lifted_to_source(lift_inline_subquery(table));
            sources.push(SourceSkeleton {
                max_denom_exclusive: max_denom,
                source,
                filter,
                geometry_column: gc.clone(),
                id_column: id_col.clone(),
                attributes: Vec::new(),
            });
        }
    } else if let Some(table) = from_table {
        let (source, filter) = lifted_to_source(lift_inline_subquery(&table));
        sources.push(SourceSkeleton {
            max_denom_exclusive: max_scale_denom,
            source,
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

/// parse a mapfile SYMBOL definition body into a `SymbolDef`. recognises:
///
/// - TYPE ELLIPSE -> Circle
/// - TYPE HATCH -> Hatch (with ANGLE/SIZE defaults)
/// - TYPE VECTOR with POINTS body -> VectorShape (filled / anchored)
/// - TYPE VECTOR without POINTS but with a known shape NAME -> NamedShape
/// - TYPE TRUETYPE -> Glyph (FONT + CHARACTER)
///
/// other TYPEs (PIXMAP) are dropped with a warn at use site.
fn parse_symbol(body: &[Token]) -> Option<(String, SymbolDef)> {
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
        let kw = t.keyword.to_ascii_uppercase();
        match kw.as_str() {
            "NAME" if name.is_none() => name = t.args.first().cloned(),
            "TYPE" if type_.is_none() => type_ = t.args.first().cloned(),
            "ANGLE" => angle_deg = parsing::first_parsed(t),
            "SIZE" => size = parsing::first_parsed(t),
            "FILLED" => {
                if let Some(arg) = t.args.first() {
                    filled = matches!(arg.to_ascii_uppercase().as_str(), "TRUE" | "ON" | "YES" | "1");
                }
            }
            "POINTS" => {
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
            "ANCHORPOINT" => {
                let coords = parsing::nums(t);
                if coords.len() >= 2 {
                    anchor = Some((coords[0], coords[1]));
                }
            }
            "FONT" => font = parsing::first_unquoted(t),
            "CHARACTER" => character = parsing::first_unquoted(t),
            _ => {}
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
        _ => return None,
    };
    Some((name, def))
}

/// translate a `LiftedBinding` into the `BindingSource` shape the emitter
/// consumes plus the optional filter expression. `Sql` bindings carry no
/// filter through this path - the operator already inlined any WHERE clause
/// into the SELECT.
fn lifted_to_source(lifted: LiftedBinding) -> (BindingSource, Option<String>) {
    match lifted {
        LiftedBinding::Table { table, filter } => (BindingSource::Table(table), filter),
        LiftedBinding::Sql { sql } => {
            tracing::warn!(
                "DATA could not be lifted to a single-table binding; emitting as sql: \
                 (snapshot compile is not yet wired for sql bindings - operator must \
                 either review or wait for follow-up)"
            );
            (BindingSource::Sql(sql), None)
        }
    }
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

/// Outcome of lifting a mapfile DATA / SCALETOKEN binding into MARS shape.
/// The simple table+filter case is preferred; anything beyond that lands as
/// a raw `sql:` binding for the operator to review.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LiftedBinding {
    Table { table: String, filter: Option<String> },
    Sql { sql: String },
}

/// Lift a mapfile inline DATA value into either a clean table binding or a
/// raw-SQL one. Recognised shapes:
///
/// - `table` or `schema.table` -> `LiftedBinding::Table` with no filter.
/// - `(SELECT ... FROM <table> WHERE <expr>) [AS alias]` -> `Table` with
///   the WHERE clause as the filter.
/// - anything else (joins, derived columns, multi-segment SELECTs) ->
///   `Sql`, preserving the raw text so the operator can hand-edit.
pub(crate) fn lift_inline_subquery(raw: &str) -> LiftedBinding {
    let trimmed = raw.trim();
    let inner = match trimmed.strip_prefix('(') {
        Some(rest) => match rest.rsplit_once(')') {
            Some((body, _tail)) => body.trim(),
            None => return LiftedBinding::Sql { sql: raw.to_string() },
        },
        None => {
            return LiftedBinding::Table {
                table: raw.to_string(),
                filter: None,
            };
        }
    };
    let upper = inner.to_ascii_uppercase();
    if !upper.trim_start().starts_with("SELECT") {
        return LiftedBinding::Sql { sql: raw.to_string() };
    }
    let from_pos = match upper.find(" FROM ") {
        Some(p) => p + " FROM ".len(),
        None => return LiftedBinding::Sql { sql: raw.to_string() },
    };
    let where_pos = upper[from_pos..].find(" WHERE ").map(|p| from_pos + p);

    let table_section = match where_pos {
        Some(wp) => &inner[from_pos..wp],
        None => &inner[from_pos..],
    };
    let table = table_section.trim().to_string();
    // accept simple `schema.table` / `table` / `"table"`; anything more
    // elaborate becomes a sql: binding so the operator sees a clean error
    // rather than a fabricated from: that the postgres adapter would reject.
    if table.contains(',') || table.contains(' ') || table.contains('(') {
        return LiftedBinding::Sql { sql: raw.to_string() };
    }
    let cleaned_table = table.trim_matches('"').to_string();
    let where_clause = match where_pos {
        Some(wp) => inner[wp + " WHERE ".len()..].trim(),
        None => "",
    };
    if where_clause.is_empty() {
        return LiftedBinding::Table {
            table: cleaned_table,
            filter: None,
        };
    }
    // round-trip through the mars-expr parser when feasible to normalise
    // quoting/spacing. round-trip failure means the expression is outside
    // the DSL; preserve the raw text so config-validation reports it.
    let normalised = mars_expr::parse(where_clause)
        .map(|e| e.to_string())
        .unwrap_or_else(|_| where_clause.to_string());
    LiftedBinding::Table {
        table: cleaned_table,
        filter: Some(normalised),
    }
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
        match style.fill {
            Some(crate::emitter::EmitFill::Hatch {
                spacing,
                angle_deg,
                line_width,
                colour,
            }) => {
                assert!((spacing - 4.0).abs() < f32::EPSILON);
                assert!((angle_deg - 45.0).abs() < f32::EPSILON);
                assert!((line_width - 0.5).abs() < f32::EPSILON);
                assert_eq!(colour, mars_style::Colour::rgb(100, 110, 120));
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

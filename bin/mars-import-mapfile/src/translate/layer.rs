//! LAYER block parser. Walks a layer body, dispatches CLASS / LABEL /
//! SCALETOKEN sub-blocks, and emits a [`LayerSkeleton`] into the
//! [`Skeleton`]. Owns the mapfile DATA -> binding lifting helpers.

use std::collections::{BTreeSet, HashSet};

use tracing::warn;

use crate::directive::LayerDirective;
use crate::emitter::{BindingSource, ClassSkeleton, LabelSkeleton, LayerSkeleton, Skeleton, SourceSkeleton};
use crate::parsing;
use crate::scanner::{Token, block_range, is_block_opener};
use crate::translate::{is_unsupported, normalize_n_plus_one};

use super::class::parse_class;
use super::label::parse_label;

pub(crate) fn handle_layer(
    body: &[Token],
    layer_line: usize,
    skel: &mut Skeleton,
    include_layers: Option<&HashSet<String>>,
) {
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
        match LayerDirective::from_token(t, is_unsupported) {
            LayerDirective::Name(t) if name.is_none() => name = t.args.first().cloned(),
            LayerDirective::Title(t) if title.is_none() => title = t.args.first().cloned(),
            LayerDirective::Type(t) if layer_type.is_none() => layer_type = t.args.first().cloned(),
            LayerDirective::Data(t) if data.is_none() => data = Some(t.args.join(" ")),
            LayerDirective::ClassItem(t) if class_item.is_none() => class_item = parsing::first_unquoted(t),
            LayerDirective::LabelItem(t) if label_item.is_none() => label_item = parsing::first_unquoted(t),
            LayerDirective::MinScaleDenom(t) => {
                if let Some(n) = parse_scale_denom_arg(t) {
                    _min_scale_denom = Some(n);
                }
            }
            LayerDirective::MaxScaleDenom(t) => {
                if let Some(n) = parse_scale_denom_arg(t) {
                    max_scale_denom = Some(n);
                }
            }
            LayerDirective::Processing(t) => {
                if let Some(arg) = t.args.first() {
                    let up = arg.to_ascii_uppercase();
                    if let Some(rest) = up.strip_prefix("ITEMS=") {
                        processing_items = Some(rest.to_string());
                    }
                }
            }
            LayerDirective::ScaleToken => {
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
            LayerDirective::Class(t) => {
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
            LayerDirective::Label(t) => {
                if let Some(r) = block_range(body, i) {
                    let resolved_name = name.clone().unwrap_or_else(|| format!("unnamed_layer_l{layer_line}"));
                    if let Some(lbl) = parse_label(&body[r.start + 1..r.end - 1], t.line, &resolved_name, skel) {
                        label = Some(lbl);
                    }
                    i = r.end;
                    continue;
                }
            }
            LayerDirective::Unsupported(t) => {
                warn!(line = t.line, keyword = %t.keyword, "unsupported mapfile construct");
                if is_block_opener(&t.keyword)
                    && let Some(r) = block_range(body, i)
                {
                    i = r.end;
                    continue;
                }
            }
            // re-occurrence of a wins-once scalar (NAME / TITLE / TYPE / DATA
            // / CLASSITEM / LABELITEM) after the first is ignored; same for
            // anything outside the known directive set.
            LayerDirective::Name(_)
            | LayerDirective::Title(_)
            | LayerDirective::Type(_)
            | LayerDirective::Data(_)
            | LayerDirective::ClassItem(_)
            | LayerDirective::LabelItem(_)
            | LayerDirective::Unknown => {}
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

pub(crate) fn parse_scale_token(body: &[Token]) -> Vec<(u64, String)> {
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

/// parse a MIN/MAXSCALEDENOM argument, applying the N+1 canonicalisation.
/// returns `None` on missing / non-finite / negative input, warning at the
/// token's line with the rejected raw value.
fn parse_scale_denom_arg(t: &Token) -> Option<u64> {
    let arg = t.args.first()?;
    match arg.parse::<f64>() {
        Ok(v) if v.is_finite() && v >= 0.0 => Some(normalize_n_plus_one(v as u64)),
        _ => {
            warn!(line = t.line, keyword = %t.keyword, value = %arg, "could not parse scale denom");
            None
        }
    }
}

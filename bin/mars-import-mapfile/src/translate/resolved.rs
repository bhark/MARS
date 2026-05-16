//! parse -> resolve normalisation: collapses Option-heavy `ParsedX` into
//! non-Option `ResolvedX` with every default unwrapped exactly once.
//!
//! mirrors the role of `mars-render/src/prepare.rs::resolve`. callers in
//! [`super::emit`] read from `ResolvedX` and never call `.unwrap_or(default)`
//! for anything resolvable here. when a new mapfile default lands (e.g.
//! `"geometri"`, `"polygon"`, `"sans-serif"`), it lives here exactly once.

use std::collections::{BTreeSet, HashMap};

use mars_style::Colour;
use tracing::warn;

use crate::directive::ConnectionTypeToken;
use crate::emitter::{BindingSource, EmitLinePlacement, MarkerKind, SymbolDef, VectorFileBinding, slugify};
use crate::expression::parse_mapfile_expression;

use super::class::{ParsedClass, ParsedExpression};
use super::label::ParsedLabel;
use super::layer::{
    ParsedLayer, guess_id_column, lift_inline_subquery, lifted_to_source, mapfile_type_to_geom, parse_data,
};
use super::layer_metadata::{IncludeItemsParsed, LayerMetadata};
use super::style_block::{SinglePass, style_block_to_pass};
use super::symbol::ParsedSymbol;
use crate::emitter::{IncludeItemsSkeleton, LayerAttributionSkeleton, LayerGatingSkeleton, LayerWmsSkeleton};

#[derive(Debug)]
pub(crate) struct ResolvedLayer {
    pub name: String,
    pub title: Option<String>,
    pub abstract_: Option<String>,
    pub geom_kind: Option<String>,
    pub sources: Vec<ResolvedSource>,
    pub classes: Vec<ResolvedClass>,
    pub label: Option<ResolvedLabel>,
    pub attributes: Vec<String>,
    /// Slash-separated WMS group path (`/A/B/C`) or `None` when the layer
    /// hangs off the service root. Collapsed at resolve time from `GROUP`
    /// (flat) and `wms_layer_group` (hierarchical); the hierarchical form
    /// wins when both are set.
    pub group_path: Option<String>,
    pub wms: LayerWmsSkeleton,
    pub unimplemented: Vec<&'static str>,
}

#[derive(Debug)]
pub(crate) struct ResolvedSource {
    pub source: crate::emitter::BindingSource,
    pub filter: Option<String>,
    pub geometry_column: String,
    pub id_column: Option<String>,
    pub max_denom_exclusive: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct ResolvedClass {
    pub class_name: String,
    pub title: Option<String>,
    pub when: Option<String>,
    pub min_scale_denom: Option<u64>,
    pub max_scale_denom: Option<u64>,
    pub style_type: String,
    pub style_name: String,
    /// One [`SinglePass`] per parsed STYLE block, in declared order. Length
    /// 1 emits a single named [`StyleDef`] entry; length 2+ emits a
    /// `ClassStyleAttach::Passes` inline on the class.
    pub passes: Vec<SinglePass>,
    pub label: Option<ResolvedLabel>,
    pub unimplemented: Vec<&'static str>,
}

#[derive(Debug)]
pub(crate) struct ResolvedLabel {
    pub text: String,
    pub style_name: String,
    pub fill: Colour,
    pub font_family: String,
    pub font_size: f32,
    pub halo_color: Option<Colour>,
    pub halo_width: Option<f32>,
    pub priority: Option<u16>,
    pub min_distance: Option<f32>,
    pub placement_line: Option<EmitLinePlacement>,
    pub position: Option<mars_style::AnchorPosition>,
    pub offset_px: Option<(f32, f32)>,
    pub angle_deg: Option<f32>,
    pub partials: Option<bool>,
    pub force: Option<bool>,
    pub unimplemented: Vec<&'static str>,
}

#[derive(Debug)]
pub(crate) struct ResolvedSymbol {
    pub name: String,
    pub def: SymbolDef,
}

pub(crate) fn resolve_layer(
    p: ParsedLayer,
    layer_line: usize,
    symbols: &HashMap<String, SymbolDef>,
    map_projection: Option<&str>,
    strict: bool,
) -> Option<ResolvedLayer> {
    let name = p.name.clone().unwrap_or_else(|| format!("unnamed_layer_l{layer_line}"));

    // abstract parent layer: STATUS OFF + wms_enable_request denying GetMap.
    // the path-based capabilities builder reconstructs the parent <Layer>
    // element from real children's group paths, so we drop this record
    // entirely. operators relying on Title/Abstract on the parent entry
    // should add wms_group_title / wms_group_abstract follow-up support
    // (out of scope here).
    let getmap_denied = matches!(p.wms_metadata.request_gating.get_map, Some(false));
    if p.status_off && getmap_denied {
        tracing::info!(
            line = layer_line,
            layer = %name,
            "absorbed abstract parent layer into group synthesis"
        );
        return None;
    }

    if let Some(ref t) = p.layer_type {
        let up = t.to_ascii_uppercase();
        if up == "QUERY" {
            warn!(line = layer_line, layer = %name, "skipping QUERY layer (no MARS equivalent)");
            return None;
        }
        if up == "RASTER" {
            warn!(
                line = layer_line,
                layer = %name,
                data = ?p.data,
                "skipping RASTER layer (importer cannot synthesise a `raster:` block; CONNECTION / PROJECTION are not parsed)",
            );
            return None;
        }
    }

    let geom_kind_str = p.layer_type.as_deref().and_then(mapfile_type_to_geom);
    let geom_for_classes = geom_kind_str.unwrap_or("polygon");

    let class_item = p.class_item.as_deref();
    let label_item = p.label_item.as_deref();

    let classes: Vec<ResolvedClass> = p
        .classes
        .into_iter()
        .map(|pc| resolve_class(pc, &name, geom_for_classes, class_item, label_item, symbols))
        .collect();

    let label = p
        .label
        .map(|pl| resolve_label(pl, &layer_label_style_name(&name), label_item));

    let sources = match p.connection_type.as_ref() {
        Some(ConnectionTypeToken::Ogr) => resolve_ogr_source(
            &name,
            layer_line,
            p.connection.as_deref(),
            p.projection.as_deref(),
            map_projection,
            p.max_scale_denom,
            p.processing_items.as_deref(),
        ),
        Some(ConnectionTypeToken::Other(raw)) => {
            warn!(
                line = layer_line,
                layer = %name,
                connection_type = %raw,
                "unsupported CONNECTIONTYPE; skipping layer sources"
            );
            Vec::new()
        }
        Some(ConnectionTypeToken::Postgis) | None => {
            if strict && p.connection_type.is_none() && p.data.is_some() {
                // postgis is implied; surface a warn only under --strict so
                // back-compat (untyped) mapfiles don't flood stderr.
                warn!(
                    line = layer_line,
                    layer = %name,
                    "CONNECTIONTYPE missing on postgis-style DATA layer; defaulting to postgis"
                );
            }
            resolve_sources(
                p.data.as_deref(),
                &p.scale_token_values,
                p.max_scale_denom,
                p.processing_items.as_deref(),
                p.filter.as_ref(),
            )
        }
    };

    // attribute idents from class predicates, per-tier filters and label-text
    // templates - config validation requires every ident referenced by these
    // to be declared on the binding.
    let mut all_attrs: BTreeSet<String> = BTreeSet::new();
    for cls in &classes {
        if let Some(ref when) = cls.when
            && let Ok(expr) = mars_expr::parse(when)
        {
            mars_expr::collect_idents(&expr, &mut all_attrs);
        }
        if let Some(ref l) = cls.label {
            collect_template_idents(&l.text, &mut all_attrs);
        }
    }
    if let Some(ref l) = label {
        collect_template_idents(&l.text, &mut all_attrs);
    }
    for src in &sources {
        if let Some(ref f) = src.filter
            && let Ok(expr) = mars_expr::parse(f)
        {
            mars_expr::collect_idents(&expr, &mut all_attrs);
        }
    }

    // aggregate dropped-directive signals from classes and label into a
    // layer-level bag. emit-time fires a single warn summarising what was
    // lost; mirrors mars-render's `Resolved::unimplemented` pattern.
    let mut unimplemented: Vec<&'static str> = Vec::new();
    for c in &classes {
        for u in &c.unimplemented {
            if !unimplemented.contains(u) {
                unimplemented.push(*u);
            }
        }
        if let Some(ref l) = c.label {
            for u in &l.unimplemented {
                if !unimplemented.contains(u) {
                    unimplemented.push(*u);
                }
            }
        }
    }
    if let Some(ref l) = label {
        for u in &l.unimplemented {
            if !unimplemented.contains(u) {
                unimplemented.push(*u);
            }
        }
    }

    let group_path = normalize_group_path(p.wms_layer_group.as_deref(), p.group.as_deref());
    let title = p.wms_metadata.title_override.clone().or(p.title.clone());
    let abstract_ = p.wms_metadata.abstract_override.clone();
    let wms = layer_wms_skeleton(&p.wms_metadata);

    Some(ResolvedLayer {
        name,
        title,
        abstract_,
        geom_kind: geom_kind_str.map(|s| s.to_string()),
        sources,
        classes,
        label,
        attributes: all_attrs.into_iter().collect(),
        group_path,
        wms,
        unimplemented,
    })
}

/// Project the parser-side [`LayerMetadata`] into the emitter-side
/// [`LayerWmsSkeleton`]. The two shapes differ only in field naming and the
/// `IncludeItemsParsed` -> `IncludeItemsSkeleton` enum rename; everything
/// else is moved as-is.
fn layer_wms_skeleton(m: &LayerMetadata) -> LayerWmsSkeleton {
    LayerWmsSkeleton {
        keywords: m.keywords.clone(),
        metadata_urls: m
            .metadata_urls
            .iter()
            .map(|mu| (mu.type_.clone(), mu.format.clone(), mu.href.clone()))
            .collect(),
        authorities: m.authorities.clone(),
        identifiers: m.identifiers.clone(),
        opaque: m.opaque,
        advertised_crs: m.advertised_crs.clone(),
        attribution: m.attribution.as_ref().map(|a| LayerAttributionSkeleton {
            title: a.title.clone(),
            online_resource: a.online_resource.clone(),
            logo_format: a.logo_format.clone(),
            logo_href: a.logo_href.clone(),
            logo_width: a.logo_width,
            logo_height: a.logo_height,
        }),
        include_items: m.include_items.as_ref().map(|i| match i {
            IncludeItemsParsed::All => IncludeItemsSkeleton::All,
            IncludeItemsParsed::None => IncludeItemsSkeleton::None,
            IncludeItemsParsed::Explicit(names) => IncludeItemsSkeleton::Explicit(names.clone()),
        }),
        request_gating: LayerGatingSkeleton {
            get_capabilities: m.request_gating.get_capabilities,
            get_map: m.request_gating.get_map,
            get_feature_info: m.request_gating.get_feature_info,
            get_legend_graphic: m.request_gating.get_legend_graphic,
            get_styles: m.request_gating.get_styles,
            describe_layer: m.request_gating.describe_layer,
        },
    }
}

/// Collapse mapfile `GROUP` (flat) and `wms_layer_group` (hierarchical)
/// metadata into a single canonical slash-prefixed path. The hierarchical
/// form wins when both are set (MapServer convention). Empty / whitespace
/// segments are dropped, and the result is always either `None` or a path
/// like `/A/B/C`.
fn normalize_group_path(wms: Option<&str>, group: Option<&str>) -> Option<String> {
    let raw = wms.or(group)?;
    let segments: Vec<&str> = raw.split('/').map(str::trim).filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(raw.len() + 1);
    for s in segments {
        out.push('/');
        out.push_str(s);
    }
    Some(out)
}

/// Lift a `CONNECTIONTYPE OGR` layer's CONNECTION + PROJECTION into a single
/// vectorfile binding. Failure cases (missing connection / unknown
/// /vsi-prefix / unknown extension / no source CRS) warn and return an
/// empty source list.
fn resolve_ogr_source(
    layer_name: &str,
    layer_line: usize,
    connection: Option<&str>,
    layer_projection: Option<&str>,
    map_projection: Option<&str>,
    max_scale_denom: Option<u64>,
    processing_items: Option<&str>,
) -> Vec<ResolvedSource> {
    let Some(raw) = connection else {
        warn!(
            line = layer_line,
            layer = %layer_name,
            "OGR layer has no CONNECTION; skipping"
        );
        return Vec::new();
    };
    let Some(uri) = ogr_connection_to_uri(raw, layer_name, layer_line) else {
        return Vec::new();
    };
    let Some(format) = infer_vectorfile_format(&uri) else {
        warn!(
            line = layer_line,
            layer = %layer_name,
            uri = %uri,
            "OGR connection has unknown extension; cannot infer format - skipping"
        );
        return Vec::new();
    };
    let source_crs = match layer_projection.or(map_projection) {
        Some(s) => s.to_string(),
        None => {
            warn!(
                line = layer_line,
                layer = %layer_name,
                "OGR layer has no PROJECTION (and no MAP PROJECTION fallback); skipping"
            );
            return Vec::new();
        }
    };
    let id_col = processing_items.and_then(guess_id_column);
    vec![ResolvedSource {
        source: BindingSource::VectorFile(VectorFileBinding {
            uri,
            format,
            source_crs,
        }),
        filter: None,
        // vectorfile bindings ignore geometry_column at decode time, but the
        // emitter still writes it for shape parity. Keep it empty to make
        // intent explicit in the YAML.
        geometry_column: String::new(),
        id_column: id_col,
        max_denom_exclusive: max_scale_denom,
    }]
}

/// Translate a MapServer-OGR CONNECTION string into an object-store URI.
/// Returns `None` (after warning) for unrecognised `/vsi*` prefixes; bare
/// paths route to `file://` (canonicalised when relative).
fn ogr_connection_to_uri(raw: &str, layer_name: &str, layer_line: usize) -> Option<String> {
    let trimmed = raw.trim().trim_matches('"');
    if let Some(rest) = trimmed.strip_prefix("/vsis3/") {
        return Some(format!("s3://{rest}"));
    }
    if let Some(rest) = trimmed.strip_prefix("/vsigs/") {
        return Some(format!("gs://{rest}"));
    }
    if let Some(rest) = trimmed.strip_prefix("/vsicurl/") {
        return Some(rest.to_string());
    }
    if trimmed.starts_with("/vsi") {
        warn!(
            line = layer_line,
            layer = %layer_name,
            connection = %trimmed,
            "unsupported /vsi* prefix in OGR CONNECTION; skipping layer"
        );
        return None;
    }
    if trimmed.starts_with("s3://")
        || trimmed.starts_with("gs://")
        || trimmed.starts_with("file://")
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
    {
        return Some(trimmed.to_string());
    }
    // bare path: absolute -> file:///abs; relative -> canonicalise against cwd.
    let pathbuf = std::path::Path::new(trimmed);
    let abs = if pathbuf.is_absolute() {
        pathbuf.to_path_buf()
    } else {
        std::env::current_dir()
            .ok()
            .map(|cwd| cwd.join(pathbuf))
            .unwrap_or_else(|| pathbuf.to_path_buf())
    };
    Some(format!("file://{}", abs.display()))
}

/// Map a file extension on the URI's path to a `VectorFileFormat` wire spelling.
fn infer_vectorfile_format(uri: &str) -> Option<String> {
    // strip query/fragment before extension match
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".fgb") {
        Some("flat_geobuf".to_string())
    } else if lower.ends_with(".geojson") || lower.ends_with(".json") {
        Some("geo_json".to_string())
    } else {
        None
    }
}

fn resolve_sources(
    data: Option<&str>,
    scale_token_values: &[(u64, String)],
    max_scale_denom: Option<u64>,
    processing_items: Option<&str>,
    layer_filter: Option<&(String, usize)>,
) -> Vec<ResolvedSource> {
    let (geom_col, from_table) = parse_data(data);
    let id_col = processing_items.and_then(guess_id_column);

    // parse the layer-level FILTER body once. Ok = normalised DSL string;
    // Err = a TODO comment surfacing the raw text so the operator notices
    // rather than have the predicate silently dropped.
    let layer_filter: Option<Result<String, String>> =
        layer_filter.map(|(raw, line)| match parse_mapfile_expression(raw, *line) {
            Ok(expr) => Ok(format!("{expr}")),
            Err(e) => {
                warn!(line = *line, error = %e, "could not parse layer FILTER");
                Err(format!("# TODO: hand-translate FILTER: {raw}"))
            }
        });

    let combine = |inline: Option<String>| -> Option<String> {
        match (&layer_filter, inline) {
            (Some(Ok(layer)), Some(inline)) => Some(format!("({layer}) AND ({inline})")),
            (Some(Ok(layer)), None) => Some(layer.clone()),
            (Some(Err(todo)), _) => Some(todo.clone()),
            (None, inline) => inline,
        }
    };

    if !scale_token_values.is_empty() {
        let gc = geom_col.unwrap_or_else(|| "geometri".into());
        let n = scale_token_values.len();
        (0..n)
            .map(|idx| {
                let (_min, table) = &scale_token_values[idx];
                let max_denom = if idx + 1 < n {
                    Some(scale_token_values[idx + 1].0)
                } else {
                    max_scale_denom
                };
                let (source, inline_filter) = lifted_to_source(lift_inline_subquery(table));
                ResolvedSource {
                    source,
                    filter: combine(inline_filter),
                    geometry_column: gc.clone(),
                    id_column: id_col.clone(),
                    max_denom_exclusive: max_denom,
                }
            })
            .collect()
    } else if let Some(table) = from_table {
        let (source, inline_filter) = lifted_to_source(lift_inline_subquery(&table));
        vec![ResolvedSource {
            source,
            filter: combine(inline_filter),
            geometry_column: geom_col.unwrap_or_else(|| "geometri".into()),
            id_column: id_col,
            max_denom_exclusive: max_scale_denom,
        }]
    } else {
        Vec::new()
    }
}

fn resolve_class(
    p: ParsedClass,
    layer_name: &str,
    geom_kind: &str,
    class_item: Option<&str>,
    label_item: Option<&str>,
    symbols: &HashMap<String, SymbolDef>,
) -> ResolvedClass {
    let title = p.name.clone();
    let class_name = slugify(&p.name.unwrap_or_else(|| format!("class_l{}", p.class_line)));
    let style_prefix = if geom_kind == "polygon" { "poly" } else { geom_kind };
    let style_name = format!("{}_{}_{}", style_prefix, slugify(layer_name), class_name);

    let passes: Vec<SinglePass> = p.styles.iter().map(|sb| style_block_to_pass(sb, symbols)).collect();

    let when = resolve_when(p.expression, class_item, title.as_deref(), layer_name, p.class_line);

    let label = p
        .label
        .map(|pl| resolve_label(pl, &class_label_style_name(layer_name, &class_name), label_item));

    let mut unimplemented: Vec<&'static str> = Vec::new();
    for sb in &p.styles {
        for u in &sb.unimplemented {
            if !unimplemented.contains(u) {
                unimplemented.push(*u);
            }
        }
    }
    for pass in &passes {
        for u in &pass.unimplemented {
            if !unimplemented.contains(u) {
                unimplemented.push(*u);
            }
        }
    }

    ResolvedClass {
        class_name,
        title,
        when,
        min_scale_denom: p.min_scale_denom,
        max_scale_denom: p.max_scale_denom,
        style_type: geom_kind.to_string(),
        style_name,
        passes,
        label,
        unimplemented,
    }
}

/// reconcile a class's EXPRESSION shape with the layer's CLASSITEM.
///
/// `BareLiteral`, `Set`, and `Range` are CLASSITEM-relative by construction -
/// they pick up the column at this point. `Regex` is also CLASSITEM-relative
/// but emits a TODO (mars_expr has no regex AST). `Predicate` is
/// self-contained and passes through. `None` falls back to the CLASS NAME /
/// CLASSITEM expansion that has always existed for un-EXPRESSION'd classes.
fn resolve_when(
    expression: Option<ParsedExpression>,
    class_item: Option<&str>,
    title: Option<&str>,
    layer_name: &str,
    class_line: usize,
) -> Option<String> {
    match expression {
        Some(ParsedExpression::Predicate(s)) => Some(s),
        Some(ParsedExpression::BareLiteral(lit)) => match class_item {
            Some(ci) => Some(format!("{ci} = {lit}")),
            None => {
                warn!(
                    layer = %layer_name,
                    line = class_line,
                    "CLASS EXPRESSION literal without CLASSITEM - emitting TODO"
                );
                Some(format!("# TODO: bare EXPRESSION without CLASSITEM: {lit}"))
            }
        },
        Some(ParsedExpression::Set(lits)) => match (class_item, lits.is_empty()) {
            (Some(ci), false) => Some(format_in(ci, &lits)),
            (Some(_), true) => {
                warn!(layer = %layer_name, line = class_line, "CLASS EXPRESSION empty set");
                Some("# TODO: empty EXPRESSION set".to_string())
            }
            (None, _) => {
                warn!(
                    layer = %layer_name,
                    line = class_line,
                    "CLASS EXPRESSION set without CLASSITEM - emitting TODO"
                );
                Some("# TODO: EXPRESSION set requires CLASSITEM".to_string())
            }
        },
        Some(ParsedExpression::Regex(pat)) => {
            let escaped = pat.replace('\'', "''");
            match class_item {
                Some(ci) => Some(format!("# TODO: regex match: {ci} ~ '{escaped}'")),
                None => {
                    warn!(
                        layer = %layer_name,
                        line = class_line,
                        "CLASS EXPRESSION regex without CLASSITEM - emitting TODO"
                    );
                    Some(format!("# TODO: regex match without CLASSITEM: /{pat}/"))
                }
            }
        }
        Some(ParsedExpression::Range { lo, hi }) => match class_item {
            Some(ci) => Some(format_range(ci, &lo, hi.as_ref())),
            None => {
                warn!(
                    layer = %layer_name,
                    line = class_line,
                    "CLASS EXPRESSION range without CLASSITEM - emitting TODO"
                );
                let suffix = hi.as_ref().map(|h| format!("{h}")).unwrap_or_default();
                Some(format!("# TODO: range without CLASSITEM: {lo}-{suffix}"))
            }
        },
        Some(ParsedExpression::Todo(raw)) => Some(format!("# TODO: hand-translate: {raw}")),
        None => match (class_item, title) {
            (Some(item), Some(value)) => Some(format!("{item} = '{}'", value.replace('\'', "''"))),
            _ => None,
        },
    }
}

fn format_in(column: &str, lits: &[mars_expr::Literal]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(column.len() + 8 + lits.len() * 6);
    let _ = write!(s, "{column} IN (");
    for (i, lit) in lits.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        let _ = write!(s, "{lit}");
    }
    s.push(')');
    s
}

fn format_range(column: &str, lo: &mars_expr::Literal, hi: Option<&mars_expr::Literal>) -> String {
    match hi {
        Some(hi) => format!("({column} >= {lo} AND {column} <= {hi})"),
        None => format!("{column} >= {lo}"),
    }
}

fn layer_label_style_name(layer: &str) -> String {
    format!("label_{}", slugify(layer))
}

fn class_label_style_name(layer: &str, class: &str) -> String {
    format!("label_{}__{}", slugify(layer), class)
}

fn collect_template_idents(text: &str, out: &mut BTreeSet<String>) {
    if let Ok(t) = mars_expr::parse_template(text) {
        for seg in &t.segments {
            if let mars_expr::Segment::Ident(name) = seg {
                out.insert(name.clone());
            }
        }
    }
}

// Lower a mapfile LABEL TEXT arg into a MARS template string. Recognises:
// `[col]` column refs -> `{col}`, and a single `(expr)` wrapper -> strip
// outer parens (mapfile expression form). Anything else passes through
// verbatim. The translation is intentionally minimal; complex expressions
// like `(tostring([col],"%fmt"))` stay verbatim so the operator notices.
fn mapfile_text_to_template(raw: &str) -> String {
    let trimmed = raw.trim();
    let stripped = if trimmed.starts_with('(') && trimmed.ends_with(')') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    bracket_refs_to_braces(stripped)
}

fn bracket_refs_to_braces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '[' {
            out.push(c);
            continue;
        }
        // peek for an ident-shaped run terminated by ']'. fall back to
        // verbatim on anything else so we never turn unrelated bracket
        // syntax into a malformed template.
        let mut ident = String::new();
        let mut closed = false;
        while let Some(&nc) = chars.peek() {
            if nc == ']' {
                chars.next();
                closed = true;
                break;
            }
            if nc.is_ascii_alphanumeric() || nc == '_' {
                ident.push(nc);
                chars.next();
            } else {
                break;
            }
        }
        if closed && !ident.is_empty() {
            out.push('{');
            out.push_str(&ident);
            out.push('}');
        } else {
            out.push('[');
            out.push_str(&ident);
            if closed {
                out.push(']');
            }
        }
    }
    out
}

fn resolve_label(p: ParsedLabel, style_name: &str, label_item: Option<&str>) -> ResolvedLabel {
    // LABELITEM: if the LABEL block had no TEXT, the layer's labelitem
    // becomes a `{<col>}` template referencing the column. when neither
    // TEXT nor LABELITEM is set we leave text empty so the operator gets a
    // clean `text:` slot to fill in. Explicit TEXT args go through
    // [`mapfile_text_to_template`] so MapServer's `[col]` column-ref form
    // (and the `(expr)` wrapper) lowers into MARS's `{col}` template form.
    let text = match (p.text.filter(|s| !s.is_empty()), label_item) {
        (Some(t), _) => mapfile_text_to_template(&t),
        (None, Some(item)) => format!("{{{item}}}"),
        (None, None) => String::new(),
    };

    ResolvedLabel {
        text,
        style_name: style_name.to_string(),
        fill: p.color.unwrap_or(Colour::rgb(0, 0, 0)),
        font_family: p.font.unwrap_or_else(|| "sans-serif".into()),
        font_size: p.size.unwrap_or(12.0),
        halo_color: p.outlinecolor,
        halo_width: p.outlinewidth,
        priority: p.priority,
        min_distance: p.min_distance,
        placement_line: p.placement_line,
        position: p.position,
        offset_px: p.offset_px,
        angle_deg: p.angle_deg,
        partials: p.partials,
        force: p.force,
        unimplemented: p.unimplemented,
    }
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn mapfile_text_lowers_bracket_refs() {
        assert_eq!(mapfile_text_to_template("[name]"), "{name}");
        assert_eq!(mapfile_text_to_template("([name])"), "{name}");
        assert_eq!(
            mapfile_text_to_template("[short_name] - [city]"),
            "{short_name} - {city}"
        );
    }

    #[test]
    fn mapfile_text_passes_unknown_forms_through() {
        // unmatched bracket: leave intact rather than emit a half-template.
        assert_eq!(mapfile_text_to_template("[unclosed"), "[unclosed");
        // empty brackets: not an ident, pass through.
        assert_eq!(mapfile_text_to_template("[]"), "[]");
        // function-call expression form stays verbatim (operator must
        // translate the surrounding call by hand).
        assert_eq!(
            mapfile_text_to_template("(tostring([col],\"%f\"))"),
            "tostring({col},\"%f\")"
        );
    }

    #[test]
    fn group_path_collapses_flat_group_to_single_segment() {
        assert_eq!(normalize_group_path(None, Some("Basis")).as_deref(), Some("/Basis"));
    }

    #[test]
    fn group_path_collapses_hierarchical_wms_group_path() {
        assert_eq!(
            normalize_group_path(Some("/Adresse/Bygning"), None).as_deref(),
            Some("/Adresse/Bygning")
        );
        // missing leading slash still produces a normalised path.
        assert_eq!(
            normalize_group_path(Some("Adresse/Bygning"), None).as_deref(),
            Some("/Adresse/Bygning")
        );
    }

    #[test]
    fn group_path_wms_layer_group_wins_over_flat_group() {
        assert_eq!(
            normalize_group_path(Some("/A/B"), Some("Other")).as_deref(),
            Some("/A/B"),
        );
    }

    #[test]
    fn group_path_drops_empty_segments_and_returns_none_when_blank() {
        assert_eq!(normalize_group_path(Some("///A// /B/"), None).as_deref(), Some("/A/B"));
        assert!(normalize_group_path(Some(""), None).is_none());
        assert!(normalize_group_path(Some("///"), None).is_none());
    }

    use mars_expr::Literal;

    #[test]
    fn regex_with_classitem_emits_classitem_aware_todo() {
        let w = resolve_when(
            Some(ParsedExpression::Regex("^A".into())),
            Some("rtt"),
            None,
            "layer",
            1,
        );
        assert_eq!(w.as_deref(), Some("# TODO: regex match: rtt ~ '^A'"));
    }

    #[test]
    fn regex_without_classitem_emits_raw_todo() {
        let w = resolve_when(Some(ParsedExpression::Regex("foo".into())), None, None, "layer", 1);
        assert_eq!(w.as_deref(), Some("# TODO: regex match without CLASSITEM: /foo/"));
    }

    #[test]
    fn regex_pattern_with_quote_is_doubled() {
        let w = resolve_when(
            Some(ParsedExpression::Regex("o'brien".into())),
            Some("name"),
            None,
            "layer",
            1,
        );
        assert_eq!(w.as_deref(), Some("# TODO: regex match: name ~ 'o''brien'"));
    }

    #[test]
    fn closed_range_with_classitem_emits_bounded_predicate() {
        let w = resolve_when(
            Some(ParsedExpression::Range {
                lo: Literal::Int(2),
                hi: Some(Literal::Int(12)),
            }),
            Some("rtt"),
            None,
            "layer",
            1,
        );
        let s = w.unwrap();
        assert_eq!(s, "(rtt >= 2 AND rtt <= 12)");
        // round-trips through mars_expr
        mars_expr::parse(&s).unwrap();
    }

    #[test]
    fn open_upper_range_with_classitem_emits_lower_bound_only() {
        let w = resolve_when(
            Some(ParsedExpression::Range {
                lo: Literal::Int(100),
                hi: None,
            }),
            Some("rtt"),
            None,
            "layer",
            1,
        );
        let s = w.unwrap();
        assert_eq!(s, "rtt >= 100");
        mars_expr::parse(&s).unwrap();
    }

    #[test]
    fn range_without_classitem_emits_todo() {
        let w = resolve_when(
            Some(ParsedExpression::Range {
                lo: Literal::Int(2),
                hi: Some(Literal::Int(12)),
            }),
            None,
            None,
            "layer",
            1,
        );
        assert_eq!(w.as_deref(), Some("# TODO: range without CLASSITEM: 2-12"));
    }

    #[test]
    fn mixed_range_round_trips() {
        let w = resolve_when(
            Some(ParsedExpression::Range {
                lo: Literal::Int(0),
                hi: Some(Literal::Float(2.5)),
            }),
            Some("rtt"),
            None,
            "layer",
            1,
        );
        let s = w.unwrap();
        assert_eq!(s, "(rtt >= 0 AND rtt <= 2.5)");
        mars_expr::parse(&s).unwrap();
    }
}

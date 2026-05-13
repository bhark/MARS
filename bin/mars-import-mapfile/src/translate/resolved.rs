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

use crate::emitter::{EmitLinePlacement, MarkerKind, SymbolDef, slugify};

use super::class::ParsedClass;
use super::label::ParsedLabel;
use super::layer::{
    ParsedLayer, guess_id_column, lift_inline_subquery, lifted_to_source, mapfile_type_to_geom, parse_data,
};
use super::style_block::{CollapsedStyle, collapse_styles};
use super::symbol::ParsedSymbol;

#[derive(Debug)]
pub(crate) struct ResolvedLayer {
    pub name: String,
    pub title: Option<String>,
    pub geom_kind: Option<String>,
    pub sources: Vec<ResolvedSource>,
    pub classes: Vec<ResolvedClass>,
    pub label: Option<ResolvedLabel>,
    pub attributes: Vec<String>,
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
    pub collapsed: CollapsedStyle,
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
) -> Option<ResolvedLayer> {
    let name = p.name.clone().unwrap_or_else(|| format!("unnamed_layer_l{layer_line}"));

    if let Some(ref t) = p.layer_type {
        let up = t.to_ascii_uppercase();
        if up == "RASTER" || up == "QUERY" {
            warn!(line = layer_line, layer = %name, "skipping RASTER/QUERY layer");
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
        .map(|pc| resolve_class(pc, &name, geom_for_classes, class_item, symbols))
        .collect();

    let label = p.label.map(|pl| resolve_label(pl, &name, label_item));

    let sources = resolve_sources(
        p.data.as_deref(),
        &p.scale_token_values,
        p.max_scale_denom,
        p.processing_items.as_deref(),
    );

    // attribute idents from class predicates + per-tier filters - config
    // validation requires every filter ident to be declared on the binding.
    let mut all_attrs: BTreeSet<String> = BTreeSet::new();
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

    // aggregate dropped-directive signals from classes into a layer-level
    // bag. emit-time fires a single warn summarising what was lost; mirrors
    // mars-render's `Resolved::unimplemented` pattern.
    let mut unimplemented: Vec<&'static str> = Vec::new();
    for c in &classes {
        for u in &c.unimplemented {
            if !unimplemented.contains(u) {
                unimplemented.push(*u);
            }
        }
    }

    Some(ResolvedLayer {
        name,
        title: p.title,
        geom_kind: geom_kind_str.map(|s| s.to_string()),
        sources,
        classes,
        label,
        attributes: all_attrs.into_iter().collect(),
        unimplemented,
    })
}

fn resolve_sources(
    data: Option<&str>,
    scale_token_values: &[(u64, String)],
    max_scale_denom: Option<u64>,
    processing_items: Option<&str>,
) -> Vec<ResolvedSource> {
    let (geom_col, from_table) = parse_data(data);
    let id_col = processing_items.and_then(guess_id_column);

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
                let (source, filter) = lifted_to_source(lift_inline_subquery(table));
                ResolvedSource {
                    source,
                    filter,
                    geometry_column: gc.clone(),
                    id_column: id_col.clone(),
                    max_denom_exclusive: max_denom,
                }
            })
            .collect()
    } else if let Some(table) = from_table {
        let (source, filter) = lifted_to_source(lift_inline_subquery(&table));
        vec![ResolvedSource {
            source,
            filter,
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
    symbols: &HashMap<String, SymbolDef>,
) -> ResolvedClass {
    let title = p.name.clone();
    let class_name = slugify(&p.name.unwrap_or_else(|| format!("class_l{}", p.class_line)));
    let style_prefix = if geom_kind == "polygon" { "poly" } else { geom_kind };
    let style_name = format!("{}_{}_{}", style_prefix, slugify(layer_name), class_name);

    let collapsed = collapse_styles(&p.styles, p.class_line, symbols);

    // CLASSITEM expansion: a CLASS with NAME but no EXPRESSION inherits an
    // implicit `<classitem> = '<NAME>'` predicate.
    let when = p.expression.or_else(|| match (class_item, title.as_deref()) {
        (Some(item), Some(value)) => Some(format!("{item} = '{}'", value.replace('\'', "''"))),
        _ => None,
    });

    let mut unimplemented: Vec<&'static str> = Vec::new();
    for sb in &p.styles {
        for u in &sb.unimplemented {
            if !unimplemented.contains(u) {
                unimplemented.push(*u);
            }
        }
    }
    for u in &collapsed.unimplemented {
        if !unimplemented.contains(u) {
            unimplemented.push(*u);
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
        collapsed,
        unimplemented,
    }
}

fn resolve_label(p: ParsedLabel, layer_name: &str, label_item: Option<&str>) -> ResolvedLabel {
    // LABELITEM: if the LABEL block had no TEXT, the layer's labelitem
    // becomes a `{<col>}` template referencing the column. when neither
    // TEXT nor LABELITEM is set we leave text empty so the operator gets a
    // clean `text:` slot to fill in.
    let text = match (p.text.filter(|s| !s.is_empty()), label_item) {
        (Some(t), _) => t,
        (None, Some(item)) => format!("{{{item}}}"),
        (None, None) => String::new(),
    };

    ResolvedLabel {
        text,
        style_name: format!("label_{}", slugify(layer_name)),
        fill: p.color.unwrap_or(Colour::rgb(0, 0, 0)),
        font_family: p.font.unwrap_or_else(|| "sans-serif".into()),
        font_size: p.size.unwrap_or(12.0),
        halo_color: p.outlinecolor,
        halo_width: p.outlinewidth,
        priority: p.priority,
        min_distance: p.min_distance,
        placement_line: p.placement_line,
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
        other => SymbolDef::NotImplemented {
            raw_type: other.to_string(),
        },
    };
    Some(ResolvedSymbol { name, def })
}

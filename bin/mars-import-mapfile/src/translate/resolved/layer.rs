//! ResolvedLayer + the top-level `resolve_layer` entry point. Owns the
//! WMS / OWS skeleton lifts, group-path collapse, and the PostGIS DSN
//! lift. Dispatches to the source / class / label resolve modules for
//! per-entity work.

use std::collections::{BTreeSet, HashMap};

use tracing::warn;

use crate::directive::ConnectionTypeToken;
use crate::emitter::{
    IncludeItemsSkeleton, LayerAttributionSkeleton, LayerGatingSkeleton, LayerOwsSkeleton, LayerWmsSkeleton, SymbolDef,
};

use super::super::layer::{ParsedLayer, mapfile_type_to_geom};
use super::super::layer_metadata::{IncludeItemsParsed, LayerMetadata};

use super::class::{LayerComposite, ResolvedClass, resolve_class};
use super::label::{ResolvedLabel, collect_template_idents, layer_label_style_name, resolve_label};
use super::source::{ResolvedSource, resolve_ogr_source, resolve_sources};

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
    /// Lifted PostGIS DSN from `CONNECTIONTYPE POSTGIS` + `CONNECTION "<dsn>"`.
    /// `Some` only for layers that declare both; non-PostGIS layers and
    /// layers without an explicit CONNECTION leave this `None` so the
    /// MAP-scope lift can distinguish agreement, mixed input, and absence.
    pub postgis_dsn: Option<String>,
    pub wms: LayerWmsSkeleton,
    pub ows: LayerOwsSkeleton,
    /// Mapfile `TEMPLATE "path.html"` lowered for the YAML emitter. The
    /// importer passes the path through verbatim; the operator either points
    /// MARS at a `{ident}`-style template or rewrites the path as inline
    /// template text post-import.
    pub template: Option<String>,
    pub unimplemented: Vec<&'static str>,
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
    // surface as a metadata-only record (no sources/classes/label) so the
    // emitted YAML keeps title/abstract/keywords/metadata_urls on the parent
    // entry. the request_gating.get_map=false carries through to the runtime
    // and capabilities builder, which together make it non-renderable.
    let getmap_denied = matches!(p.wms_metadata.request_gating.get_map, Some(false));
    if p.status_off && getmap_denied {
        tracing::info!(
            line = layer_line,
            layer = %name,
            "translating advertise-only layer (STATUS OFF + GetMap denied)"
        );
        return Some(ResolvedLayer {
            name,
            title: p.wms_metadata.title_override.clone().or(p.title.clone()),
            abstract_: p.wms_metadata.abstract_override.clone(),
            // config requires a kind; surface polygon for the advertise-only
            // record. the layer never renders (GetMap denied), but the field
            // must parse cleanly downstream.
            geom_kind: Some("polygon".into()),
            sources: Vec::new(),
            classes: Vec::new(),
            label: None,
            attributes: Vec::new(),
            group_path: normalize_group_path(p.wms_layer_group.as_deref(), p.group.as_deref()),
            postgis_dsn: None,
            wms: layer_wms_skeleton(&p.wms_metadata),
            ows: layer_ows_skeleton(&p.wms_metadata),
            template: p.template.clone(),
            unimplemented: Vec::new(),
        });
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

    let postgis_dsn = lift_postgis_dsn(p.connection_type.as_ref(), p.connection.as_deref(), &name, layer_line);

    let geom_kind_str = p.layer_type.as_deref().and_then(mapfile_type_to_geom);
    let geom_for_classes = geom_kind_str.unwrap_or("polygon");

    let class_item = p.class_item.as_deref();
    let label_item = p.label_item.as_deref();

    let composite = LayerComposite {
        opacity: p.composite_opacity,
        blend_mode: p.composite_blend_mode,
    };
    let classes: Vec<ResolvedClass> = p
        .classes
        .into_iter()
        .map(|pc| resolve_class(pc, &name, geom_for_classes, class_item, label_item, symbols, composite))
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
    let ows = layer_ows_skeleton(&p.wms_metadata);

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
        postgis_dsn,
        wms,
        ows,
        template: p.template,
        unimplemented,
    })
}

/// Lift a layer's CONNECTIONTYPE + CONNECTION pair into a PostGIS DSN.
/// Returns `Some` only for POSTGIS layers carrying an explicit CONNECTION.
/// non-POSTGIS types log a warn so the operator notices the dropped DSN; OGR
/// is unaffected because its CONNECTION is consumed by `resolve_ogr_source`.
fn lift_postgis_dsn(kind: Option<&ConnectionTypeToken>, dsn: Option<&str>, layer: &str, line: usize) -> Option<String> {
    let dsn = dsn?;
    match kind {
        Some(ConnectionTypeToken::Postgis) => Some(dsn.to_string()),
        Some(ConnectionTypeToken::Ogr) => None,
        Some(ConnectionTypeToken::Other(raw)) => {
            warn!(line, layer, connection_type = %raw, "non-postgis CONNECTIONTYPE; CONNECTION dropped");
            None
        }
        // mapfile permits a bare CONNECTION without CONNECTIONTYPE in older
        // dialects; the type is ambiguous - skip rather than guess.
        None => {
            warn!(line, layer, "CONNECTION without CONNECTIONTYPE; dropped");
            None
        }
    }
}

/// WMS-only extras (opaque + advertised CRS list).
fn layer_wms_skeleton(m: &LayerMetadata) -> LayerWmsSkeleton {
    LayerWmsSkeleton {
        opaque: m.opaque,
        advertised_crs: m.advertised_crs.clone(),
    }
}

/// Cross-protocol OWS metadata + per-op gating. The
/// `IncludeItemsParsed` -> `IncludeItemsSkeleton` rename is the only
/// non-mechanical transform; the rest is moved as-is.
fn layer_ows_skeleton(m: &LayerMetadata) -> LayerOwsSkeleton {
    LayerOwsSkeleton {
        keywords: m.keywords.clone(),
        metadata_urls: m
            .metadata_urls
            .iter()
            .map(|mu| (mu.type_.clone(), mu.format.clone(), mu.href.clone()))
            .collect(),
        authorities: m.authorities.clone(),
        identifiers: m.identifiers.clone(),
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
            wmts_get_capabilities: m.request_gating.wmts_get_capabilities,
            wmts_get_tile: m.request_gating.wmts_get_tile,
            wmts_get_feature_info: m.request_gating.wmts_get_feature_info,
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

#[cfg(test)]
mod tests;

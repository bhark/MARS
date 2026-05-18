//! mapfile-to-YAML intermediate representation.
//!
//! data-only: the translator fills these structs in; `render` walks them.
//! style-shaped wire enums (StyleDef, EmitFill, MarkerKind, ...) live in
//! `super::style_model`.

use std::collections::HashMap;

use super::style_model::{EmitLinePlacement, StyleDef, SymbolDef};

#[derive(Debug, Default)]
pub(crate) struct Skeleton {
    pub(crate) service_name: Option<String>,
    pub(crate) service_title: Option<String>,
    /// MAP-level METADATA harvested from `ows_*` / `wms_*` keys. Populated by
    /// `parse_map_metadata`; consumed by `render` when emitting the service
    /// block. Empty defaults preserve the placeholder-only output for inputs
    /// without metadata.
    pub(crate) service_meta: ServiceMetaSkeleton,
    /// MAP-level `PROJECTION { ... }` capture (e.g. `EPSG:25832`). Falls
    /// back as source_crs for OGR layers that have no layer-scope PROJECTION
    /// block of their own. `None` when the mapfile has no MAP PROJECTION.
    pub(crate) map_projection: Option<String>,
    pub(crate) layers: Vec<LayerSkeleton>,
    pub(crate) styles: Vec<StyleDef>,
    /// mapfile-level SYMBOL definitions keyed by name. consumed by STYLE
    /// blocks via STYLE.SYMBOL "<name>"; not emitted into YAML directly -
    /// each STYLE that uses a symbol carries the resolved marker/fill on
    /// its `StyleDef`.
    pub(crate) symbols: HashMap<String, SymbolDef>,
    /// `MAP MAXSIZE n` lifted to `interfaces.wms.max_image_dimension`. None
    /// keeps the runtime adapter default.
    pub(crate) wms_max_image_dimension: Option<u32>,
    /// `MAP RESOLUTION n` lifted to `service.scale_dpi`. None falls back to
    /// the config default (96).
    pub(crate) scale_dpi: Option<f64>,
}

/// MAP-level service metadata harvested from the mapfile METADATA block.
/// Each field carries either a parsed value or the absent state; the emitter
/// is responsible for falling back to placeholders when nothing is set.
#[derive(Debug, Default)]
pub(crate) struct ServiceMetaSkeleton {
    pub(crate) title_override: Option<String>,
    pub(crate) abstract_: Option<String>,
    pub(crate) keywords: Vec<String>,
    pub(crate) online_resource: Option<String>,
    pub(crate) fees: Option<String>,
    pub(crate) access_constraints: Option<String>,
    pub(crate) encoding: Option<String>,
    pub(crate) bbox_extended: Option<bool>,
    pub(crate) sld_enabled: Option<bool>,
    pub(crate) advertised_crs: Vec<String>,
    pub(crate) contact_person: Option<String>,
    pub(crate) contact_position: Option<String>,
    pub(crate) contact_organization: Option<String>,
    pub(crate) contact_phone: Option<String>,
    pub(crate) contact_fax: Option<String>,
    pub(crate) contact_email: Option<String>,
    pub(crate) address_type: Option<String>,
    pub(crate) address_street: Option<String>,
    pub(crate) address_city: Option<String>,
    pub(crate) address_state: Option<String>,
    pub(crate) address_postcode: Option<String>,
    pub(crate) address_country: Option<String>,
    pub(crate) getmap_formats: Vec<String>,
    pub(crate) getfeatureinfo_formats: Vec<String>,
    pub(crate) getlegend_formats: Vec<String>,
    pub(crate) authorities: Vec<(String, String)>,
    pub(crate) identifiers: Vec<(String, String)>,
}

impl ServiceMetaSkeleton {
    /// True when any structured contact sub-field is set. Drives whether the
    /// emitter writes a full `contact:` block or keeps the top-level
    /// `contact_email` shorthand.
    pub(crate) fn has_structured_contact(&self) -> bool {
        self.contact_person.is_some()
            || self.contact_position.is_some()
            || self.contact_organization.is_some()
            || self.contact_phone.is_some()
            || self.contact_fax.is_some()
            || self.address_type.is_some()
            || self.address_street.is_some()
            || self.address_city.is_some()
            || self.address_state.is_some()
            || self.address_postcode.is_some()
            || self.address_country.is_some()
    }
}

#[derive(Debug, Default)]
pub(crate) struct LayerSkeleton {
    pub(crate) name: String,
    pub(crate) title: Option<String>,
    pub(crate) abstract_: Option<String>,
    pub(crate) geom_kind: Option<String>,
    pub(crate) sources: Vec<SourceSkeleton>,
    pub(crate) classes: Vec<ClassSkeleton>,
    pub(crate) label: Option<LabelSkeleton>,
    /// Slash-prefixed WMS group path (`/A/B/C`). `None` puts the layer at
    /// the service root.
    pub(crate) group: Option<String>,
    /// Per-layer WMS-only extras (opaque, advertised CRS) harvested from
    /// `METADATA { ... }`.
    pub(crate) wms: LayerWmsSkeleton,
    /// Per-layer OWS metadata + cross-protocol gating.
    pub(crate) ows: LayerOwsSkeleton,
    /// PostGIS DSN lifted from this layer's `CONNECTION`. folded at render
    /// time into a single MAP-scope `source.dsn` when every PostGIS layer
    /// agrees; see [`super::dsn::fold_postgis_dsns`].
    pub(crate) postgis_dsn: Option<String>,
    /// Mapfile `TEMPLATE "path.html"`, threaded into the emitted YAML's
    /// `template:` field for the GetFeatureInfo template renderer.
    pub(crate) template: Option<String>,
}

/// Per-layer WMS-only extras (opaque, advertised CRS list). The cross-
/// protocol metadata that used to live here moved to [`LayerOwsSkeleton`]
/// when gating was generalised across OWS-family interfaces.
#[derive(Debug, Default)]
pub(crate) struct LayerWmsSkeleton {
    pub(crate) opaque: Option<bool>,
    pub(crate) advertised_crs: Vec<String>,
}

impl LayerWmsSkeleton {
    pub(crate) fn is_empty(&self) -> bool {
        self.opaque.is_none() && self.advertised_crs.is_empty()
    }
}

/// Per-layer OWS metadata that flows from the importer into the emitted
/// YAML's per-layer `ows:` block. Each field is absent by default and the
/// emitter writes only fields with content.
#[derive(Debug, Default)]
pub(crate) struct LayerOwsSkeleton {
    pub(crate) keywords: Vec<String>,
    pub(crate) metadata_urls: Vec<(String, String, String)>, // (type, format, href)
    pub(crate) authorities: Vec<(String, String)>,
    pub(crate) identifiers: Vec<(String, String)>,
    pub(crate) attribution: Option<LayerAttributionSkeleton>,
    pub(crate) include_items: Option<IncludeItemsSkeleton>,
    pub(crate) request_gating: LayerGatingSkeleton,
}

impl LayerOwsSkeleton {
    pub(crate) fn is_empty(&self) -> bool {
        self.keywords.is_empty()
            && self.metadata_urls.is_empty()
            && self.authorities.is_empty()
            && self.identifiers.is_empty()
            && self.attribution.is_none()
            && self.include_items.is_none()
            && self.request_gating.is_empty()
    }
}

#[derive(Debug, Default)]
pub(crate) struct LayerAttributionSkeleton {
    pub(crate) title: Option<String>,
    pub(crate) online_resource: Option<String>,
    pub(crate) logo_format: Option<String>,
    pub(crate) logo_href: Option<String>,
    pub(crate) logo_width: Option<u32>,
    pub(crate) logo_height: Option<u32>,
}

#[derive(Debug)]
pub(crate) enum IncludeItemsSkeleton {
    All,
    None,
    Explicit(Vec<String>),
}

#[derive(Debug, Default)]
pub(crate) struct LayerGatingSkeleton {
    pub(crate) get_capabilities: Option<bool>,
    pub(crate) get_map: Option<bool>,
    pub(crate) get_feature_info: Option<bool>,
    pub(crate) get_legend_graphic: Option<bool>,
    pub(crate) get_styles: Option<bool>,
    pub(crate) describe_layer: Option<bool>,
    pub(crate) wmts_get_capabilities: Option<bool>,
    pub(crate) wmts_get_tile: Option<bool>,
    pub(crate) wmts_get_feature_info: Option<bool>,
}

impl LayerGatingSkeleton {
    pub(crate) fn is_empty(&self) -> bool {
        self.get_capabilities.is_none()
            && self.get_map.is_none()
            && self.get_feature_info.is_none()
            && self.get_legend_graphic.is_none()
            && self.get_styles.is_none()
            && self.describe_layer.is_none()
            && self.wmts_get_capabilities.is_none()
            && self.wmts_get_tile.is_none()
            && self.wmts_get_feature_info.is_none()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SourceSkeleton {
    pub(crate) max_denom_exclusive: Option<u64>,
    /// Either a table reference (`from:`) or an inline SELECT (`sql:`).
    pub(crate) source: BindingSource,
    pub(crate) filter: Option<String>,
    pub(crate) geometry_column: String,
    pub(crate) id_column: Option<String>,
    pub(crate) attributes: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum BindingSource {
    /// `from: <table>` form.
    Table(String),
    /// `sql: <SELECT ...>` form for inline subqueries the mapfile DATA block
    /// could not be lifted to a plain table reference. Snapshot-only.
    Sql(String),
    /// vectorfile binding lifted from a `CONNECTIONTYPE OGR` + `CONNECTION`
    /// pair. emits `uri: / format: / source_crs:` on the per-binding line.
    VectorFile(VectorFileBinding),
}

/// vectorfile binding fields. format is the snake_case wire spelling
/// (`flat_geobuf` | `geo_json`); source_crs is the layer's PROJECTION (with
/// MAP-level fallback) in `EPSG:NNNN` form.
#[derive(Debug, Clone)]
pub(crate) struct VectorFileBinding {
    pub uri: String,
    pub format: String,
    pub source_crs: String,
}

impl SourceSkeleton {
    /// Diagnostic descriptor for the binding's table reference. Used by
    /// tests that inspect the produced source layout. SQL bindings collapse
    /// to a sentinel so existing assertions read straight through.
    #[cfg(test)]
    pub(crate) fn source_table(&self) -> &str {
        match &self.source {
            BindingSource::Table(t) => t.as_str(),
            BindingSource::Sql(_) => "<sql>",
            BindingSource::VectorFile(_) => "<vectorfile>",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ClassSkeleton {
    pub(crate) name: String,
    pub(crate) title: Option<String>,
    pub(crate) when: Option<String>,
    pub(crate) min_scale_denom: Option<u64>,
    pub(crate) max_scale_denom: Option<u64>,
    /// Per-class style attachment. Single-pass classes route through the
    /// styles registry via `Ref`; multi-pass classes emit inline `Passes` so
    /// the per-block ordering survives without coupling the named-style
    /// registry to the pass ordering.
    pub(crate) style: ClassStyleAttach,
    pub(crate) label: Option<LabelSkeleton>,
}

/// Style attachment shape on a [`ClassSkeleton`]. Mirrors
/// `mars_config::ClassStyle` minus the `Inline` variant the importer never
/// emits.
#[derive(Debug, Clone)]
pub(crate) enum ClassStyleAttach {
    /// Reference to a named [`StyleDef`] in [`Skeleton::styles`].
    Ref(String),
    /// Ordered inline multi-pass stack. Each [`StyleDef`] carries the
    /// per-pass body verbatim; the `style_type` field is shared across
    /// passes and matches the class's geometry kind.
    Passes(Vec<StyleDef>),
}

#[derive(Debug, Clone)]
pub(crate) struct LabelSkeleton {
    pub(crate) text: String,
    pub(crate) style_ref: String,
    /// `ANGLE FOLLOW` (mapserver) -> `placement: { kind: line, ... }`.
    pub(crate) placement_line: Option<EmitLinePlacement>,
}

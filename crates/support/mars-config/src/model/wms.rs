//! WMS-shaped configuration types.
//!
//! Holds the per-layer ([`LayerWms`]) and per-service ([`ServiceWms`])
//! protocol-metadata aggregates plus the leaf types that compose them. All
//! fields here surface into WMS GetCapabilities or per-request gating; they
//! never affect rendering or storage. New WMS metadata fields belong here -
//! not on [`super::Layer`] or [`super::ServiceMeta`].

use serde::{Deserialize, Serialize};

/// `(name, href)` pair for `<AuthorityURL>` elements. Used at both the
/// service/root-layer scope (via [`ServiceWms::authorities`]) and per-layer
/// scope (via [`LayerWms::authorities`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthorityRef {
    pub name: String,
    pub href: String,
}

/// `(authority, value)` pair for `<Identifier authority="...">value</Identifier>`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdentifierRef {
    pub authority: String,
    pub value: String,
}

/// `<MetadataURL>` entry for a layer. `type_` carries the content-spec
/// (e.g., `"ISO19115:2003"`, `"FGDC:1998"`), `format` the MIME type of the
/// linked document, and `href` the URL. Mirrors MapServer
/// `wms_metadataurl_type` / `_format` / `_href` triples.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetadataUrl {
    #[serde(rename = "type")]
    pub type_: String,
    pub format: String,
    pub href: String,
}

/// `<Attribution>` block for a layer. All fields optional.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Attribution {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub online_resource: Option<String>,
    #[serde(default)]
    pub logo: Option<LogoUrl>,
}

/// `<LogoURL format="..." width="..." height="..."><OnlineResource ../></LogoURL>`
/// surfaced from MapServer `wms_attribution_logourl_*` keys.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LogoUrl {
    pub format: String,
    pub href: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

/// `ows_include_items` policy controlling which attributes flow into
/// GetFeatureInfo (and future WFS) output. Default is `All`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncludeItems {
    #[serde(default)]
    pub mode: IncludeMode,
    /// Attribute names; only meaningful when `mode == Explicit`.
    #[serde(default)]
    pub names: Vec<String>,
}

impl Default for IncludeItems {
    fn default() -> Self {
        Self {
            mode: IncludeMode::All,
            names: Vec::new(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IncludeMode {
    #[default]
    All,
    None,
    Explicit,
}

/// Per-operation advertised format lists. Each list is consulted by the WMS
/// capabilities emitter; empty falls back to legacy resolution (the
/// renderable list from `interfaces.wms.formats` for GetMap/GetLegendGraphic,
/// the hardcoded info-formats constant for GetFeatureInfo). MapServer keys:
/// `wms_getmap_formatlist`, `wms_feature_info_mime_type`,
/// `wms_getlegendgraphic_formatlist`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceFormats {
    #[serde(default)]
    pub get_map: Vec<String>,
    #[serde(default)]
    pub get_feature_info: Vec<String>,
    #[serde(default)]
    pub get_legend_graphic: Vec<String>,
}

/// Per-layer WMS-only extras. Cross-protocol metadata (keywords, metadata
/// URLs, authorities, identifiers, attribution, include-items, per-op
/// gating) lives on [`super::LayerOws`] instead. This block carries the
/// fields whose semantics are WMS-specific: `<Opaque>`, the per-layer
/// `<CRS>` list, and the legacy GFI opt-in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LayerWms {
    /// MapServer `wms_opaque`. When true the layer is advertised as
    /// non-transparent (clients composite it as a base layer).
    #[serde(default)]
    pub opaque: bool,
    /// Per-layer advertised CRS list. None = inherit `service.wms.advertised_crs`
    /// (in 1.3.0 layers inherit root-layer CRSes when this is empty).
    #[serde(default)]
    pub advertised_crs: Option<Vec<String>>,
    /// Whether GFI is permitted on this layer when no explicit
    /// `ows.request_gating.wms_get_feature_info` override is set. Legacy
    /// opt-in default kept because GFI's spec-default is deny; new configs
    /// should prefer the explicit OWS gating form.
    #[serde(default)]
    pub enable_get_feature_info: bool,
}

/// Service-level WMS-only metadata. Anything WMTS or other OWS-family
/// protocols also consume lives on [`super::ServiceOws`] instead.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceWms {
    /// MapServer `ows_bbox_extended`: when true, WMS 1.3.0 GetMap accepts and
    /// emits BBOX with explicit CRS axis ordering hints.
    #[serde(default)]
    pub bbox_extended: bool,
    /// Service-level advertised CRS list (`ows_srs` / `wms_srs`). Per-layer
    /// `LayerWms::advertised_crs` overrides this. Empty = fall back to
    /// native_crs only.
    #[serde(default)]
    pub advertised_crs: Vec<String>,
    /// MapServer `wms_sld_enabled`. False = MARS does not advertise SLD
    /// support (current default - MARS has no SLD implementation).
    #[serde(default)]
    pub sld_enabled: bool,
    /// `ows_authorityurl_*` pairs surfaced as root-layer `<AuthorityURL>` in
    /// WMS 1.3.0 capabilities.
    #[serde(default)]
    pub authorities: Vec<AuthorityRef>,
    /// `ows_identifier_*` pairs surfaced as root-layer `<Identifier>` in
    /// WMS 1.3.0 capabilities.
    #[serde(default)]
    pub identifiers: Vec<IdentifierRef>,
    /// Per-operation advertised format lists. Each list is empty by default;
    /// emitters fall back to legacy behavior (interfaces.wms.formats for
    /// GetMap/GetLegendGraphic, hardcoded INFO_FORMATS for GetFeatureInfo).
    #[serde(default)]
    pub formats: ServiceFormats,
}

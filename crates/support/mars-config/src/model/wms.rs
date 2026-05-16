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

/// WMS operations subject to per-layer gating. Wire form matches the
/// MapServer `wms_enable_request` token names (case-insensitive on parse).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WmsOperation {
    GetCapabilities,
    GetMap,
    GetFeatureInfo,
    GetLegendGraphic,
    GetStyles,
    DescribeLayer,
}

/// Per-operation allow/deny set. `Some(true)` allows, `Some(false)` denies,
/// `None` falls through to the layer's default-allow (or
/// [`LayerWms::enable_get_feature_info`] for GFI). Wire form: a single block
/// with named boolean keys per operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestGating {
    #[serde(default)]
    pub get_capabilities: Option<bool>,
    #[serde(default)]
    pub get_map: Option<bool>,
    #[serde(default)]
    pub get_feature_info: Option<bool>,
    #[serde(default)]
    pub get_legend_graphic: Option<bool>,
    #[serde(default)]
    pub get_styles: Option<bool>,
    #[serde(default)]
    pub describe_layer: Option<bool>,
}

impl RequestGating {
    /// Returns the explicit gating decision for `op`. `None` means no override
    /// was set in config - callers fall through to the operation's default.
    #[must_use]
    pub fn allowed(&self, op: WmsOperation) -> Option<bool> {
        match op {
            WmsOperation::GetCapabilities => self.get_capabilities,
            WmsOperation::GetMap => self.get_map,
            WmsOperation::GetFeatureInfo => self.get_feature_info,
            WmsOperation::GetLegendGraphic => self.get_legend_graphic,
            WmsOperation::GetStyles => self.get_styles,
            WmsOperation::DescribeLayer => self.describe_layer,
        }
    }
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

/// Per-layer WMS metadata. Covers everything the WMS capabilities emitter
/// reads off a layer plus per-operation gating consulted at request time.
/// Defaults to an empty/permissive block; missing `wms:` in YAML yields
/// [`LayerWms::default`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LayerWms {
    /// Per-layer keywords surfaced in WMS `<KeywordList>`. Empty = element
    /// omitted.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// `wms_metadataurl_*` entries surfaced as `<MetadataURL>` blocks on the
    /// layer. Each entry pairs a content type with a format and href.
    #[serde(default)]
    pub metadata_urls: Vec<MetadataUrl>,
    /// Per-layer `<AuthorityURL>` entries. Override the service-level set on a
    /// per-layer basis; an empty list keeps the service-scoped behavior in
    /// WMS 1.3.0 (root-layer inheritance).
    #[serde(default)]
    pub authorities: Vec<AuthorityRef>,
    /// Per-layer `<Identifier>` entries (1.3.0 inherits these from the root
    /// layer by default; per-layer entries override).
    #[serde(default)]
    pub identifiers: Vec<IdentifierRef>,
    /// MapServer `wms_opaque`. When true the layer is advertised as
    /// non-transparent (clients composite it as a base layer).
    #[serde(default)]
    pub opaque: bool,
    /// Per-layer advertised CRS list. None = inherit `service.wms.advertised_crs`
    /// (in 1.3.0 layers inherit root-layer CRSes when this is empty).
    #[serde(default)]
    pub advertised_crs: Option<Vec<String>>,
    /// MapServer `wms_attribution_*` block surfaced as `<Attribution>` on the
    /// layer.
    #[serde(default)]
    pub attribution: Option<Attribution>,
    /// MapServer `ows_include_items`: which attributes flow into
    /// GetFeatureInfo (and future WFS) output. Default = `All`.
    #[serde(default)]
    pub include_items: IncludeItems,
    /// Per-operation allow/deny gating. An explicit `Some(false)` for an
    /// operation denies it for this layer; `None` falls back to default-allow
    /// (with the exception of `GetFeatureInfo`, where
    /// [`Self::enable_get_feature_info`] remains the legacy opt-in default for
    /// backward compatibility).
    #[serde(default)]
    pub request_gating: RequestGating,
    /// Whether GFI is permitted on this layer when no explicit
    /// `request_gating.get_feature_info` override is set. Predates
    /// `request_gating`; kept as the legacy opt-in default for GFI gating.
    #[serde(default)]
    pub enable_get_feature_info: bool,
}

impl LayerWms {
    /// Resolved gating decision for `op`. `GetFeatureInfo` keeps the legacy
    /// opt-in default via [`Self::enable_get_feature_info`] when no explicit
    /// override is present; all other ops default-allow.
    #[must_use]
    pub fn permits_wms_op(&self, op: WmsOperation) -> bool {
        match op {
            WmsOperation::GetFeatureInfo => self.request_gating.allowed(op).unwrap_or(self.enable_get_feature_info),
            other => self.request_gating.allowed(other).unwrap_or(true),
        }
    }
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn bare_wms() -> LayerWms {
        LayerWms::default()
    }

    #[test]
    fn default_gating_allows_getmap_and_blocks_gfi() {
        let w = bare_wms();
        // default-allow for GetMap, GetCapabilities, GetLegendGraphic etc.
        assert!(w.permits_wms_op(WmsOperation::GetMap));
        assert!(w.permits_wms_op(WmsOperation::GetCapabilities));
        assert!(w.permits_wms_op(WmsOperation::GetLegendGraphic));
        // GFI keeps the legacy opt-in default (false) when no override is set
        assert!(!w.permits_wms_op(WmsOperation::GetFeatureInfo));
    }

    #[test]
    fn enable_gfi_flips_default_gfi_gating() {
        let mut w = bare_wms();
        w.enable_get_feature_info = true;
        assert!(w.permits_wms_op(WmsOperation::GetFeatureInfo));
    }

    #[test]
    fn explicit_gating_overrides_defaults() {
        let mut w = bare_wms();
        // override GFI=true even though enable_get_feature_info=false
        w.request_gating.get_feature_info = Some(true);
        assert!(w.permits_wms_op(WmsOperation::GetFeatureInfo));
        // explicit deny for GetMap
        w.request_gating.get_map = Some(false);
        assert!(!w.permits_wms_op(WmsOperation::GetMap));
    }

    #[test]
    fn deny_get_capabilities_is_explicit() {
        let mut w = bare_wms();
        w.request_gating.get_capabilities = Some(false);
        assert!(!w.permits_wms_op(WmsOperation::GetCapabilities));
    }
}

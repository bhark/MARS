//! OWS Common metadata shared across capabilities-emitting protocols
//! (WMS + WMTS today; WCS / WFS later). Anything WMS-only lives on
//! [`super::ServiceWms`] instead.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::wms::{Attribution, AuthorityRef, IdentifierRef, IncludeItems, MetadataUrl};

/// Service-level OWS metadata. Surfaced into both WMS and WMTS capabilities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceOws {
    /// Service-level keywords surfaced in WMS `<KeywordList>` and WMTS
    /// `ows:Keywords`. Empty = element omitted.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Public URL used as `OnlineResource` href on the service block and
    /// per-operation `DCPType/HTTP/Get`. None = element omitted.
    #[serde(default)]
    pub online_resource: Option<String>,
    /// Free-text fees clause, MapServer `ows_fees`. None = element omitted.
    #[serde(default)]
    pub fees: Option<String>,
    /// Free-text access-constraints clause, MapServer `ows_accessconstraints`.
    #[serde(default)]
    pub access_constraints: Option<String>,
    /// XML processing-instruction `encoding="..."`. None = "UTF-8".
    #[serde(default)]
    pub encoding: Option<String>,
}

impl ServiceOws {
    /// XML encoding to emit in the capabilities document declaration.
    /// Defaults to UTF-8 when unset.
    #[must_use]
    pub fn xml_encoding(&self) -> &str {
        self.encoding.as_deref().unwrap_or("UTF-8")
    }
}

/// Service operations subject to per-layer gating. Spans every OWS-family
/// protocol MARS exposes today; new protocols extend the enum rather than
/// growing a parallel enum + parallel gating map. Wire form is the variant
/// name in snake_case (`wms_get_map`, `wmts_get_tile`) so YAML stays
/// grep-able.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceOp {
    WmsGetMap,
    WmsGetCapabilities,
    WmsGetFeatureInfo,
    WmsGetLegendGraphic,
    WmsGetStyles,
    WmsDescribeLayer,
    WmtsGetTile,
    WmtsGetCapabilities,
    WmtsGetFeatureInfo,
}

impl ServiceOp {
    /// Short stable identifier matching the wire form. Used in error
    /// messages so operators can correlate denials with their YAML.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WmsGetMap => "wms_get_map",
            Self::WmsGetCapabilities => "wms_get_capabilities",
            Self::WmsGetFeatureInfo => "wms_get_feature_info",
            Self::WmsGetLegendGraphic => "wms_get_legend_graphic",
            Self::WmsGetStyles => "wms_get_styles",
            Self::WmsDescribeLayer => "wms_describe_layer",
            Self::WmtsGetTile => "wmts_get_tile",
            Self::WmtsGetCapabilities => "wmts_get_capabilities",
            Self::WmtsGetFeatureInfo => "wmts_get_feature_info",
        }
    }
}

/// Per-layer OWS metadata + cross-protocol request gating. Fields here are
/// shared across every OWS-family protocol; WMS-only extras (opaque,
/// advertised CRS list, GFI legacy opt-in) live on [`super::LayerWms`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LayerOws {
    /// Per-layer keywords surfaced into both WMS `<KeywordList>` and WMTS
    /// `ows:Keywords`. Empty = element omitted.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// `wms_metadataurl_*` entries surfaced as `<MetadataURL>` blocks on the
    /// layer. Each entry pairs a content type with a format and href.
    #[serde(default)]
    pub metadata_urls: Vec<MetadataUrl>,
    /// Per-layer `<AuthorityURL>` entries. Override the service-level set on
    /// a per-layer basis.
    #[serde(default)]
    pub authorities: Vec<AuthorityRef>,
    /// Per-layer `<Identifier>` entries (1.3.0 inherits these from the root
    /// layer by default; per-layer entries override).
    #[serde(default)]
    pub identifiers: Vec<IdentifierRef>,
    /// MapServer `wms_attribution_*` block surfaced as `<Attribution>` on the
    /// layer.
    #[serde(default)]
    pub attribution: Option<Attribution>,
    /// MapServer `ows_include_items`: which attributes flow into
    /// GetFeatureInfo (and future WFS) output. Default = `All`.
    #[serde(default)]
    pub include_items: IncludeItems,
    /// Per-operation allow/deny gating. Absence of a key means the
    /// operation's default-allow applies (with a single exception for
    /// `WmsGetFeatureInfo`, which falls back to
    /// [`super::LayerWms::enable_get_feature_info`] for back-compat).
    #[serde(default)]
    pub request_gating: BTreeMap<ServiceOp, bool>,
}

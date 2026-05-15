use serde::{Deserialize, Serialize};

/// Service identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceMeta {
    /// Service slug used in URLs and manifest paths.
    pub name: String,
    /// Human-readable title shown in capabilities documents.
    #[serde(default)]
    pub title: String,
    /// Long-form abstract.
    #[serde(default, rename = "abstract")]
    pub abstract_: String,
    /// Operator contact email. Kept as a top-level shorthand for the common
    /// case; subsumed by [`Self::contact`] when a full contact block is set.
    #[serde(default)]
    pub contact_email: String,
    /// Font discovery for label rendering.
    #[serde(default)]
    pub fonts: Fonts,
    /// DPI used to compute the scale denominator from a request's
    /// `(bbox, width)`. Affects WMS layer routing against `MAXSCALEDENOM`-
    /// equivalent thresholds in `scales.bands` and per-source `scale`
    /// windows. Defaults to **96**, matching MapServer's `RESOLUTION 96`
    /// convention and typical web display assumptions; the OGC reference
    /// is 90.7142857 (0.28 mm/pixel). WMTS scale denominators are spec-
    /// fixed at the OGC reference and ignore this field.
    #[serde(default = "default_scale_dpi")]
    pub scale_dpi: f64,

    /// Service-level keywords surfaced in WMS `<KeywordList>` and WMTS
    /// `ows:Keywords`. Empty = element omitted.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Public URL used as `OnlineResource` href on the service block and
    /// per-operation `DCPType/HTTP/Get`. Empty = element omitted.
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
    /// MapServer `ows_bbox_extended`: when true, WMS 1.3.0 GetMap accepts and
    /// emits BBOX with explicit CRS axis ordering hints.
    #[serde(default)]
    pub bbox_extended: bool,
    /// Service-level advertised CRS list (`ows_srs` / `wms_srs`). Per-layer
    /// `advertised_crs` overrides this. Empty = fall back to native_crs only.
    #[serde(default)]
    pub advertised_crs: Vec<String>,
    /// MapServer `wms_sld_enabled`. False = MARS does not advertise SLD
    /// support (current default - MARS has no SLD implementation).
    #[serde(default)]
    pub sld_enabled: bool,
    /// Full contact block. Empty fields are omitted from the emitted XML.
    /// Email here takes precedence over [`Self::contact_email`] when set.
    #[serde(default)]
    pub contact: ContactInfo,
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

impl Default for ServiceMeta {
    fn default() -> Self {
        Self {
            name: String::new(),
            title: String::new(),
            abstract_: String::new(),
            contact_email: String::new(),
            fonts: Fonts::default(),
            scale_dpi: default_scale_dpi(),
            keywords: Vec::new(),
            online_resource: None,
            fees: None,
            access_constraints: None,
            encoding: None,
            bbox_extended: false,
            advertised_crs: Vec::new(),
            sld_enabled: false,
            contact: ContactInfo::default(),
            authorities: Vec::new(),
            identifiers: Vec::new(),
            formats: ServiceFormats::default(),
        }
    }
}

impl ServiceMeta {
    /// Standardised pixel size in metres derived from [`Self::scale_dpi`]:
    /// `0.0254 / dpi`. The denominator helper multiplies image-width by this
    /// to recover the ground-pixel size at the configured DPI.
    #[must_use]
    pub fn scale_pixel_size_m(&self) -> f64 {
        0.0254 / self.scale_dpi
    }

    /// Resolved contact email: prefers the structured [`ContactInfo::email`]
    /// when set, otherwise falls back to the legacy top-level field.
    #[must_use]
    pub fn effective_contact_email(&self) -> &str {
        if !self.contact.email.is_empty() {
            &self.contact.email
        } else {
            &self.contact_email
        }
    }

    /// XML encoding to emit in the capabilities document declaration.
    /// Defaults to UTF-8 when unset.
    #[must_use]
    pub fn xml_encoding(&self) -> &str {
        self.encoding.as_deref().unwrap_or("UTF-8")
    }
}

/// Default DPI for the scale-denominator helper. Matches MapServer's common
/// `RESOLUTION 96` configuration and the dominant web-display DPI; deviates
/// from the OGC reference (90.7142857) intentionally for parity with the
/// most prevalent existing WMS deployments.
fn default_scale_dpi() -> f64 {
    96.0
}

/// Font discovery configuration. Controls which directories the renderer
/// scans for TrueType faces, and whether the vendored DejaVu Sans fallback
/// is registered last so labels never depend on system fontconfig.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fonts {
    /// Directories to walk for `.ttf` / `.otf` faces.
    #[serde(default)]
    pub paths: Vec<String>,
    /// When true, append the vendored DejaVu Sans fallback. Defaults to true.
    #[serde(default = "default_bundle_default")]
    pub bundle_default: bool,
}

impl Default for Fonts {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            bundle_default: default_bundle_default(),
        }
    }
}

fn default_bundle_default() -> bool {
    true
}

/// Full operator contact block. All fields optional; empty fields drop out of
/// the emitted XML. Mirrors MapServer's `ows_contact*` / `ows_address*` keys.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContactInfo {
    #[serde(default)]
    pub person: String,
    #[serde(default)]
    pub position: String,
    #[serde(default)]
    pub organization: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub fax: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub address: Address,
}

impl ContactInfo {
    /// True when no contact field carries content. Emitters skip the block
    /// entirely in that case.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.person.is_empty()
            && self.position.is_empty()
            && self.organization.is_empty()
            && self.phone.is_empty()
            && self.fax.is_empty()
            && self.email.is_empty()
            && self.address.is_empty()
    }
}

/// Postal address fields. Matches MapServer `ows_address*` keys 1:1.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Address {
    #[serde(default, rename = "type")]
    pub type_: String,
    #[serde(default)]
    pub street: String,
    #[serde(default)]
    pub city: String,
    #[serde(default)]
    pub state_or_province: String,
    #[serde(default)]
    pub postcode: String,
    #[serde(default)]
    pub country: String,
}

impl Address {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.type_.is_empty()
            && self.street.is_empty()
            && self.city.is_empty()
            && self.state_or_province.is_empty()
            && self.postcode.is_empty()
            && self.country.is_empty()
    }
}

/// `(name, href)` pair for `<AuthorityURL>` elements. Used at both the
/// service/root-layer scope (via [`ServiceMeta::authorities`]) and per-layer
/// scope (when added to the layer model in part C).
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

/// Per-operation advertised format lists. Each list is consulted by the WMS
/// capabilities emitter; empty falls back to legacy resolution (the
/// renderable list from `interfaces.wms.formats` for GetMap/GetLegendGraphic,
/// the hardcoded `INFO_FORMATS` constant for GetFeatureInfo). MapServer keys:
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

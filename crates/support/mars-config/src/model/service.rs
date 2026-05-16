use serde::{Deserialize, Serialize};

use super::ows::ServiceOws;
use super::wms::ServiceWms;

/// Service identity. Carries the stable name, human-readable title/abstract,
/// operator contact and rendering settings; capabilities-shaped metadata
/// lives on [`Self::ows`] (cross-protocol) and [`Self::wms`] (WMS-only).
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
    /// Full contact block. Empty fields are omitted from the emitted XML.
    /// Email here takes precedence over [`Self::contact_email`] when set.
    #[serde(default)]
    pub contact: ContactInfo,
    /// OWS metadata shared across capabilities-emitting protocols (WMS, WMTS,
    /// future WCS/WFS). Default-empty when omitted.
    #[serde(default)]
    pub ows: ServiceOws,
    /// WMS-only service metadata. Default-empty when omitted.
    #[serde(default)]
    pub wms: ServiceWms,
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
            contact: ContactInfo::default(),
            ows: ServiceOws::default(),
            wms: ServiceWms::default(),
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

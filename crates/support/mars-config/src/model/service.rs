use serde::{Deserialize, Serialize};

/// Service identity. SPEC §5.2.
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
    /// Operator contact email.
    #[serde(default)]
    pub contact_email: String,
    /// Font discovery for label rendering. SPEC §14.
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

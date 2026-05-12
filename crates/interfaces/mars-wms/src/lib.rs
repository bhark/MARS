//! WMS 1.3.0 interface adapter.
//!
//! Covers `GetMap`, `GetCapabilities`, and `GetFeatureInfo`. SLD / SLD_BODY
//! and GetLegendGraphic land in follow-up commits.

#![forbid(unsafe_code)]

mod capabilities;
mod exception;
mod feature_info;
mod parse;

use mars_config::Config;
use mars_runtime::RenderPlan;
use mars_types::{CrsCode, ImageFormat, LayerId};

pub use capabilities::capabilities_xml;
pub use exception::service_exception_report;
pub use feature_info::format_feature_info;
pub use parse::{parse_get_feature_info, parse_get_map, parse_request};

#[derive(Debug, thiserror::Error)]
pub enum WmsError {
    #[error("missing parameter: {0}")]
    MissingParam(&'static str),
    #[error("invalid parameter `{name}`: {reason}")]
    InvalidParam { name: &'static str, reason: String },
    #[error("not implemented: {what}")]
    NotImplemented { what: String },
}

/// hard upper bound on image dimensions to prevent oom from malicious
/// width / height parameters.
const DEFAULT_MAX_IMAGE_DIMENSION: u32 = 8192;

/// hard upper bound on `width * height` per request. tighter than
/// `max_image_dimension²` so a 8192×8192 single-axis-max stays legal but a
/// `width = 8192, height = 8192` request (256 MiB raw) trips this first.
/// 16M ≈ 4096² ≈ 64 MiB raw pixmap.
const DEFAULT_MAX_PIXELS: u64 = 16_000_000;

/// hard upper bound on layers per request to prevent excessive allocation
/// and artifact fetches.
const DEFAULT_MAX_LAYERS: usize = 100;

/// hard upper bound on absolute bbox coordinates to prevent infinite cell
/// enumeration from astronomical values.
const DEFAULT_MAX_BBOX_COORD: f64 = 1e9;

/// Per-request configuration distilled from the service [`Config`]. The edge
/// builds this once at startup and passes it by reference per request.
#[derive(Debug, Clone)]
pub struct WmsConfig {
    /// CRSes the runtime accepts on the wire (intersected with reprojection
    /// allowlist). Empty disables enforcement.
    pub allowlist_crs: Vec<CrsCode>,
    /// Output formats the runtime advertises and accepts. Empty disables
    /// enforcement.
    pub formats: Vec<ImageFormat>,
    /// maximum allowed width or height in pixels.
    pub max_image_dimension: u32,
    /// maximum allowed `width * height` per request.
    pub max_pixels: u64,
    /// maximum number of layers per getmap request.
    pub max_layers: usize,
    /// maximum absolute value of any bbox coordinate.
    pub max_bbox_coord: f64,
    /// Standardised pixel size in metres derived from `service.scale_dpi`.
    /// Drives the OGC scale-denominator calc; an in-request `&DPI=`
    /// parameter overrides this per-request.
    pub scale_pixel_size_m: f64,
}

impl WmsConfig {
    /// Derive a [`WmsConfig`] from the service config. Defaults to PNG when
    /// the YAML omits `interfaces.wms.formats`.
    #[must_use]
    pub fn from_config(cfg: &Config) -> Self {
        let allowlist_crs = cfg.reprojection.allowlist.clone();
        let wms = cfg.interfaces.wms.as_ref();
        let formats = wms
            .map(|w| {
                w.formats
                    .iter()
                    .filter_map(|f| match f.as_str() {
                        "image/png" => Some(ImageFormat::Png),
                        "image/jpeg" | "image/jpg" => Some(ImageFormat::Jpeg),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec![ImageFormat::Png]);
        Self {
            allowlist_crs,
            formats,
            max_image_dimension: wms
                .and_then(|w| w.max_image_dimension)
                .unwrap_or(DEFAULT_MAX_IMAGE_DIMENSION),
            max_pixels: wms.and_then(|w| w.max_pixels).unwrap_or(DEFAULT_MAX_PIXELS),
            max_layers: DEFAULT_MAX_LAYERS,
            max_bbox_coord: DEFAULT_MAX_BBOX_COORD,
            scale_pixel_size_m: cfg.service.scale_pixel_size_m(),
        }
    }
}

/// WMS `EXCEPTIONS=` selection. Drives the error-response format when a
/// GetMap request fails after parsing succeeds. `Xml` is the spec default;
/// `Blank` suppresses XML and returns a transparent image of the requested
/// dimensions. `INIMAGE` is intentionally not modelled - it requires
/// drawing error text via the renderer and is deferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExceptionsFormat {
    /// `EXCEPTIONS=XML` (default). ServiceExceptionReport XML payload.
    #[default]
    Xml,
    /// `EXCEPTIONS=BLANK`. Empty image at the requested dimensions, 200 OK.
    Blank,
}

/// INFO_FORMAT= selection for GetFeatureInfo responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoFormat {
    /// `text/plain`. Newline-separated `layer | user_id | k=v ...` lines.
    TextPlain,
    /// `text/html`. Per-layer tables with one row per feature.
    TextHtml,
    /// `application/json`. `{ "features": [{ "layer", "id", "attrs" }] }`.
    ApplicationJson,
}

impl InfoFormat {
    /// MIME string for the response `Content-Type` header.
    #[must_use]
    pub fn mime(self) -> &'static str {
        match self {
            Self::TextPlain => "text/plain; charset=utf-8",
            Self::TextHtml => "text/html; charset=utf-8",
            Self::ApplicationJson => "application/json",
        }
    }
}

/// Parsed GetFeatureInfo request. Carries the underlying [`RenderPlan`] (used
/// to resolve binding + level the same way GetMap would), plus the GFI-specific
/// hit-test inputs.
#[derive(Debug, Clone)]
pub struct GfiPlan {
    /// Render plan reconstructed from the GetMap params. `plan.layers` holds
    /// `QUERY_LAYERS` (a subset of the original `LAYERS`), since the GFI hit
    /// test runs only against query layers.
    pub plan: RenderPlan,
    /// Pixel-space x coordinate of the click (origin top-left).
    pub i: u32,
    /// Pixel-space y coordinate of the click (origin top-left).
    pub j: u32,
    /// Negotiated info-format. Falls back to `TextPlain` when omitted.
    pub info_format: InfoFormat,
    /// Maximum feature hits to return; spec default 1.
    pub feature_count: u32,
}

/// Hard upper bound on `FEATURE_COUNT=` to prevent runaway responses on
/// dense pages.
pub const MAX_FEATURE_COUNT: u32 = 1000;

/// Top-level WMS request taxonomy.
#[derive(Debug)]
pub enum WmsRequest {
    /// `request=GetMap` with a parsed [`RenderPlan`] and the EXCEPTIONS=
    /// selection captured at parse time.
    GetMap {
        /// Parsed render plan.
        plan: RenderPlan,
        /// EXCEPTIONS= behaviour. Default `Xml` when the param is absent.
        exceptions: ExceptionsFormat,
    },
    /// `request=GetFeatureInfo` with the parsed [`GfiPlan`].
    GetFeatureInfo(GfiPlan),
    /// `request=GetCapabilities`.
    GetCapabilities,
}

/// Layer IDs the WMS interface considers queryable. Used by the dispatcher to
/// reject `QUERY_LAYERS=` entries that name non-queryable layers per
/// capabilities.
pub fn queryable_layer_ids(cfg: &Config) -> Vec<LayerId> {
    cfg.layers
        .iter()
        .filter(|l| l.enable_get_feature_info)
        .map(|l| l.name.clone())
        .collect()
}

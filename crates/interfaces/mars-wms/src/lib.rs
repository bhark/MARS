//! WMS 1.3.0 interface adapter. SPEC §12.
//!
//! Phase 0 covers `GetMap` and `GetCapabilities` only. SLD / SLD_BODY,
//! GetFeatureInfo and GetLegendGraphic are deferred.

#![forbid(unsafe_code)]

mod capabilities;
mod exception;
mod parse;

use mars_config::Config;
use mars_runtime::RenderPlan;
use mars_types::{CrsCode, ImageFormat};

pub use capabilities::capabilities_xml;
pub use exception::service_exception_report;
pub use parse::{parse_get_map, parse_request};

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
        }
    }
}

/// Top-level WMS request taxonomy.
#[derive(Debug)]
pub enum WmsRequest {
    /// `request=GetMap` with a parsed [`RenderPlan`].
    GetMap(RenderPlan),
    /// `request=GetCapabilities`.
    GetCapabilities,
}

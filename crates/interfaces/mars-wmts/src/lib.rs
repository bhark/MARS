//! WMTS 1.0.0 interface adapter. SPEC §13.

#![forbid(unsafe_code)]

mod parse;

use std::collections::BTreeMap;

use mars_config::{Config, TileMatrixSet};
use mars_runtime::RenderPlan;
use mars_types::ImageFormat;

pub use parse::{parse_get_tile, parse_request};

#[derive(Debug, thiserror::Error)]
pub enum WmtsError {
    #[error("missing parameter: {0}")]
    MissingParam(&'static str),
    #[error("invalid parameter `{name}`: {reason}")]
    InvalidParam { name: &'static str, reason: String },
    #[error("not implemented: {what}")]
    NotImplemented { what: String },
}

/// hard upper bound on absolute bbox coordinates to prevent infinite cell
/// enumeration from astronomical computed values (mirrors the WMS guard).
const DEFAULT_MAX_BBOX_COORD: f64 = 1e9;

/// Per-request configuration distilled from the service [`Config`]. Built once
/// at startup; the edge passes it by reference per request.
#[derive(Debug, Clone)]
pub struct WmtsConfig {
    /// Named tile-matrix-set definitions exposed by this service.
    pub tile_matrix_sets: BTreeMap<String, TileMatrixSet>,
    /// Output formats accepted on the wire. Empty disables enforcement.
    pub formats: Vec<ImageFormat>,
    /// Maximum absolute value of any computed bbox coordinate.
    pub max_bbox_coord: f64,
}

impl WmtsConfig {
    /// Derive a [`WmtsConfig`] from the service config. Defaults to PNG when
    /// the YAML omits `interfaces.wmts.formats`. Restricts the exposed TMS
    /// set to the names listed under `interfaces.wmts.tile_matrix_sets`; if
    /// that list is empty, all configured sets are exposed.
    #[must_use]
    pub fn from_config(cfg: &Config) -> Self {
        let wmts = cfg.interfaces.wmts.as_ref();
        let formats = wmts
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

        let allow: Option<Vec<&str>> = wmts.map(|w| w.tile_matrix_sets.iter().map(String::as_str).collect());
        let tile_matrix_sets = cfg
            .tile_matrix_sets
            .iter()
            .filter(|(name, _)| match &allow {
                Some(names) if !names.is_empty() => names.contains(&name.as_str()),
                _ => true,
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        Self {
            tile_matrix_sets,
            formats,
            max_bbox_coord: DEFAULT_MAX_BBOX_COORD,
        }
    }
}

/// Top-level WMTS request taxonomy.
#[derive(Debug)]
pub enum WmtsRequest {
    /// `request=GetTile` with a parsed [`RenderPlan`].
    GetTile(RenderPlan),
    /// `request=GetCapabilities`.
    GetCapabilities,
}

/// Tile-cache key shape: `(layer_set, style_set, z, x, y, format)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub layer_set: String,
    pub style_set: String,
    pub z: u32,
    pub x: u32,
    pub y: u32,
    pub format: ImageFormat,
}

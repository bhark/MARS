//! WMTS 1.0.0 interface adapter.

#![forbid(unsafe_code)]

mod capabilities;
mod exception;
mod parse;
mod prepare;

pub use capabilities::capabilities_xml;
pub use exception::ows_exception_report;

use std::collections::BTreeMap;

use mars_config::{Config, ServiceOp, TileMatrixSet};
use mars_types::{ImageFormat, LayerId};

pub use parse::{parse_get_tile, parse_request, parse_rest_get_tile};
pub use prepare::ResolvedGetTile;

#[derive(Debug, thiserror::Error)]
pub enum WmtsError {
    #[error("missing parameter: {0}")]
    MissingParam(&'static str),
    #[error("invalid parameter `{name}`: {reason}")]
    InvalidParam { name: &'static str, reason: String },
    #[error("not implemented: {what}")]
    NotImplemented { what: String },
    #[error("layer `{layer}` does not permit operation {op}", op = op.as_str())]
    OperationNotPermitted { layer: LayerId, op: ServiceOp },
}

impl mars_ows_common::OwsParseError for WmtsError {
    fn missing(name: &'static str) -> Self {
        Self::MissingParam(name)
    }
    fn invalid(name: &'static str, reason: String) -> Self {
        Self::InvalidParam { name, reason }
    }
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
    /// Resolved per-layer WMTS request gating, distilled from
    /// [`mars_config::Layer::permits_op`] at startup. Unknown layers (the
    /// request names a layer not in config) are absent; [`Self::permits`]
    /// returns `true` for them so the downstream layer-existence check owns
    /// that error.
    pub layer_policies: BTreeMap<LayerId, WmtsLayerPolicy>,
}

/// Per-layer WMTS operation allow/deny flags, distilled from
/// `Layer::permits_op` at startup. Mirrors the WMS `LayerPolicy` shape so
/// the two `Config::from_config` bodies stay structurally aligned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WmtsLayerPolicy {
    pub get_tile: bool,
    pub get_capabilities: bool,
    pub get_feature_info: bool,
}

impl WmtsConfig {
    /// Derive a [`WmtsConfig`] from the service config. Defaults to PNG when
    /// the YAML omits `interfaces.wmts.formats`. Restricts the exposed TMS
    /// set to the names listed under `interfaces.wmts.tile_matrix_sets`; if
    /// that list is empty, all configured sets are exposed.
    #[must_use]
    pub fn from_config(cfg: &Config) -> Self {
        let wmts = cfg.interfaces.wmts.as_ref();
        let formats_in = wmts.map(|w| w.formats.as_slice()).unwrap_or(&[]);
        let formats = mars_ows_common::configured_formats(formats_in, ImageFormat::Png);

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

        let layer_policies = cfg
            .layers
            .iter()
            .map(|l| {
                (
                    l.name.clone(),
                    WmtsLayerPolicy {
                        get_tile: l.permits_op(ServiceOp::WmtsGetTile),
                        get_capabilities: l.permits_op(ServiceOp::WmtsGetCapabilities),
                        get_feature_info: l.permits_op(ServiceOp::WmtsGetFeatureInfo),
                    },
                )
            })
            .collect();

        Self {
            tile_matrix_sets,
            formats,
            max_bbox_coord: DEFAULT_MAX_BBOX_COORD,
            layer_policies,
        }
    }

    /// True when `layer` permits `op`. Unknown layers return `true`; the
    /// downstream layer-existence check owns that error path. Non-WMTS ops
    /// passed here default-allow; per-protocol gating lives on each
    /// interface's own `Config::permits`.
    #[must_use]
    pub fn permits(&self, layer: &LayerId, op: ServiceOp) -> bool {
        let Some(p) = self.layer_policies.get(layer) else {
            return true;
        };
        match op {
            ServiceOp::WmtsGetTile => p.get_tile,
            ServiceOp::WmtsGetCapabilities => p.get_capabilities,
            ServiceOp::WmtsGetFeatureInfo => p.get_feature_info,
            _ => true,
        }
    }
}

/// Top-level WMTS request taxonomy.
#[derive(Debug)]
pub enum WmtsRequest {
    /// `request=GetTile` with a fully-resolved render plan.
    GetTile(ResolvedGetTile),
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

#[cfg(test)]
mod policy_tests;

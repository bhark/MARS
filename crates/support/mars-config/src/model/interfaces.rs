use serde::{Deserialize, Serialize};

use crate::ConfigError;
use crate::units;

/// External interface toggles.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Interfaces {
    /// WMS endpoint config.
    #[serde(default)]
    pub wms: Option<WmsConfig>,
    /// WMTS endpoint config.
    #[serde(default)]
    pub wmts: Option<WmtsConfig>,
    /// Final tile cache config.
    #[serde(default)]
    pub tile_cache: Option<TileCacheConfig>,
}

/// WMS endpoint configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WmsConfig {
    /// Whether the endpoint is mounted.
    pub enabled: bool,
    /// Supported WMS versions.
    #[serde(default)]
    pub versions: Vec<String>,
    /// Supported MIME formats.
    #[serde(default)]
    pub formats: Vec<String>,
    /// Optional `host:port` to bind the WMS HTTP edge on. When unset the bin
    /// falls back to `MARS_HTTP_LISTEN` and finally `0.0.0.0:8080`.
    #[serde(default)]
    pub listen: Option<String>,
    /// Maximum width or height in pixels per GetMap request. Adapter default
    /// applies when unset.
    #[serde(default)]
    pub max_image_dimension: Option<u32>,
    /// Maximum `width * height` per GetMap request. Adapter default applies
    /// when unset.
    #[serde(default)]
    pub max_pixels: Option<u64>,
}

/// WMTS endpoint configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WmtsConfig {
    /// Whether the endpoint is mounted.
    pub enabled: bool,
    /// Supported WMTS versions.
    #[serde(default)]
    pub versions: Vec<String>,
    /// Tile matrix set names exposed.
    #[serde(default)]
    pub tile_matrix_sets: Vec<String>,
    /// Supported MIME formats.
    #[serde(default)]
    pub formats: Vec<String>,
}

/// Final tile cache configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileCacheConfig {
    /// Whether the tile cache is enabled.
    pub enabled: bool,
    /// Cache directory.
    pub path: String,
    /// Max disk size (unit-suffixed).
    pub max_size: String,
}

impl TileCacheConfig {
    /// Resolve `max_size` to bytes.
    pub fn max_size_bytes(&self) -> Result<u64, ConfigError> {
        units::parse_bytes(&self.max_size)
    }
}

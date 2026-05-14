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
    /// HTTP CORS policy. Absent disables CORS (today's behaviour); present
    /// mounts the configured origin / method allowlist on every route.
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

/// CORS policy. `allow_origins = ["*"]` advertises a wildcard origin;
/// otherwise the listed exact origins are reflected when the request
/// `Origin` header matches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorsConfig {
    /// Permitted origins. Use `["*"]` for a wildcard policy or an explicit
    /// list of `scheme://host[:port]` strings. Empty = effectively no
    /// origins allowed; configure `interfaces.cors: null` to disable
    /// instead.
    pub allow_origins: Vec<String>,
    /// HTTP methods exposed to cross-origin requests. Defaults to GET and
    /// HEAD which is what WMS / WMTS need.
    #[serde(default = "default_cors_methods")]
    pub allow_methods: Vec<String>,
    /// `Access-Control-Max-Age` value in seconds. `None` omits the header
    /// (browsers fall back to their own default, typically 5 seconds).
    #[serde(default)]
    pub max_age_seconds: Option<u64>,
}

fn default_cors_methods() -> Vec<String> {
    vec!["GET".to_owned(), "HEAD".to_owned()]
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

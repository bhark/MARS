use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ConfigError;
use crate::units;

/// PNG deflate level. Mirrors `png::Compression` so the adapter can map it
/// without depending on this crate. `Fast` is the right default for ephemeral
/// tile output: ~5-10x quicker than `Balanced` for ~10-15% larger payloads.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PngCompression {
    /// No compression. Largest files, fastest encode.
    None,
    /// Lightest compression (≈ deflate level 1 via fdeflate's fast path).
    Fastest,
    /// Solid speed/ratio tradeoff suited to ephemeral tile responses.
    #[default]
    Fast,
    /// Default of the `png` crate (≈ deflate level 6).
    Balanced,
    /// Smallest output, slowest encode.
    High,
}

/// Renderer / encoder configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Render {
    /// JPEG quality, 1-100. Defaults to 85.
    #[serde(default = "default_jpeg_quality")]
    pub jpeg_quality: u8,
    /// Total in-flight raw-pixmap memory budget across all concurrent renders,
    /// expressed as a unit-suffixed byte literal (`512MiB`). The runtime
    /// converts to a permit count of pixels (bytes / 4) and a render reserves
    /// `width * height` permits for its lifetime. When unset, the runtime
    /// self-sizes against the active cgroup memory limit: 40% of the limit
    /// minus a 256 MiB reservation. Outside a cgroup the fallback is 512 MiB.
    #[serde(default)]
    pub pixel_budget: Option<String>,
    /// PNG deflate level. Defaults to `fast`; `balanced` matches the upstream
    /// `png` crate default if exact byte parity with older renders is needed.
    #[serde(default)]
    pub png_compression: PngCompression,
    /// Maximum number of page artifacts fetched concurrently per layer
    /// during a single render. The render and GFI paths preserve page-key
    /// order across the fetch fan-out, so this caps in-flight store /
    /// cache pressure without affecting determinism. Must be `>= 1`.
    #[serde(default = "default_page_fetch_concurrency")]
    pub page_fetch_concurrency: usize,
    /// HTTP client knobs for XYZ raster sources. One client is shared across
    /// every XYZ-backed raster collection in the process.
    #[serde(default)]
    pub xyz_client: XyzClient,
}

/// HTTP client configuration for the XYZ raster source adapter. The adapter
/// itself imposes no defaults; the bin's composition root reads these values
/// and feeds them into the `reqwest::ClientBuilder`. Durations are humantime
/// strings (`"30s"`, `"1min"`); the empty string falls back to the default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XyzClient {
    /// End-to-end request timeout (sent -> body received). Defaults to `30s`.
    #[serde(default = "default_xyz_timeout")]
    pub timeout: String,
    /// TCP / TLS connect timeout. Defaults to `10s`.
    #[serde(default = "default_xyz_connect_timeout")]
    pub connect_timeout: String,
    /// User-Agent header sent on every tile request. Public XYZ servers (OSM,
    /// Stadia, ...) typically require an identifying UA and may 429 / 403 a
    /// missing one. Defaults to `"mars-tile-fetcher/<crate-version>"`.
    #[serde(default = "default_xyz_user_agent")]
    pub user_agent: String,
}

impl Default for XyzClient {
    fn default() -> Self {
        Self {
            timeout: default_xyz_timeout(),
            connect_timeout: default_xyz_connect_timeout(),
            user_agent: default_xyz_user_agent(),
        }
    }
}

impl XyzClient {
    /// Resolve `timeout` to a `Duration`.
    pub fn timeout(&self) -> Result<Duration, ConfigError> {
        units::parse_duration(&self.timeout)
    }

    /// Resolve `connect_timeout` to a `Duration`.
    pub fn connect_timeout(&self) -> Result<Duration, ConfigError> {
        units::parse_duration(&self.connect_timeout)
    }
}

impl Default for Render {
    fn default() -> Self {
        Self {
            jpeg_quality: default_jpeg_quality(),
            pixel_budget: None,
            png_compression: PngCompression::default(),
            page_fetch_concurrency: default_page_fetch_concurrency(),
            xyz_client: XyzClient::default(),
        }
    }
}

impl Render {
    /// Resolve `pixel_budget` to a permit count (raw pixels) when explicitly
    /// set. Saturates at `u32::MAX`. `None` means defer to cgroup auto-sizing.
    pub fn pixel_budget_permits(&self) -> Result<Option<u32>, ConfigError> {
        self.pixel_budget
            .as_deref()
            .map(|s| {
                let pixels = units::parse_bytes(s)? / 4;
                Ok(u32::try_from(pixels).unwrap_or(u32::MAX))
            })
            .transpose()
    }
}

fn default_jpeg_quality() -> u8 {
    85
}

fn default_page_fetch_concurrency() -> usize {
    16
}

fn default_xyz_timeout() -> String {
    "30s".to_owned()
}

fn default_xyz_connect_timeout() -> String {
    "10s".to_owned()
}

fn default_xyz_user_agent() -> String {
    concat!("mars-tile-fetcher/", env!("CARGO_PKG_VERSION")).to_owned()
}

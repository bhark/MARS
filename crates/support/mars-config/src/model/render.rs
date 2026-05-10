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
    /// `width * height` permits for its lifetime.
    #[serde(default = "default_pixel_budget")]
    pub pixel_budget: String,
    /// PNG deflate level. Defaults to `fast`; `balanced` matches the upstream
    /// `png` crate default if exact byte parity with older renders is needed.
    #[serde(default)]
    pub png_compression: PngCompression,
    /// **Deprecated.** Bytes-bounded LRU of decoded source-artifact geometry.
    /// The current renderer does not honour this knob; the value is parsed
    /// for backward compatibility and otherwise ignored.
    #[serde(default = "default_decoded_geometry_cache")]
    pub decoded_geometry_cache: String,
    /// **Deprecated.** Parallel geometry-emit toggle from the cell-substrate
    /// renderer. Ignored under the page-keyed substrate; accepted for
    /// backward compatibility.
    #[serde(default)]
    pub parallel_emit: ParallelEmit,
    /// Maximum number of page artifacts fetched concurrently per layer
    /// during a single render. The render and GFI paths preserve page-key
    /// order across the fetch fan-out, so this caps in-flight store /
    /// cache pressure without affecting determinism. Must be `>= 1`.
    #[serde(default = "default_page_fetch_concurrency")]
    pub page_fetch_concurrency: usize,
}

/// Configuration for the parallel geometry-emit pass.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ParallelEmit {
    /// Enable parallel dispatch. When `false`, emit runs serially on the
    /// calling worker (the pre-Phase-2 path).
    #[serde(default = "default_parallel_emit_enabled")]
    pub enabled: bool,
    /// Minimum chunk size handed to each rayon worker. Below this threshold
    /// rayon coalesces work to keep dispatch overhead off the tiny-payload
    /// hot path.
    #[serde(default = "default_parallel_emit_chunk_size")]
    pub chunk_size: usize,
}

impl Default for ParallelEmit {
    fn default() -> Self {
        Self {
            enabled: default_parallel_emit_enabled(),
            chunk_size: default_parallel_emit_chunk_size(),
        }
    }
}

fn default_parallel_emit_enabled() -> bool {
    true
}

fn default_parallel_emit_chunk_size() -> usize {
    8
}

impl Default for Render {
    fn default() -> Self {
        Self {
            jpeg_quality: default_jpeg_quality(),
            pixel_budget: default_pixel_budget(),
            png_compression: PngCompression::default(),
            decoded_geometry_cache: default_decoded_geometry_cache(),
            parallel_emit: ParallelEmit::default(),
            page_fetch_concurrency: default_page_fetch_concurrency(),
        }
    }
}

impl Render {
    /// Resolve `pixel_budget` to permit count (raw pixels). Saturates at u32::MAX.
    pub fn pixel_budget_permits(&self) -> Result<u32, ConfigError> {
        let bytes = units::parse_bytes(&self.pixel_budget)?;
        let pixels = bytes / 4;
        Ok(u32::try_from(pixels).unwrap_or(u32::MAX))
    }

    /// Resolve `decoded_geometry_cache` to a byte budget. Saturates at usize::MAX.
    pub fn decoded_geometry_cache_bytes(&self) -> Result<usize, ConfigError> {
        let bytes = units::parse_bytes(&self.decoded_geometry_cache)?;
        Ok(usize::try_from(bytes).unwrap_or(usize::MAX))
    }
}

fn default_jpeg_quality() -> u8 {
    85
}

fn default_pixel_budget() -> String {
    "512MiB".to_owned()
}

fn default_decoded_geometry_cache() -> String {
    "256MiB".to_owned()
}

fn default_page_fetch_concurrency() -> usize {
    16
}

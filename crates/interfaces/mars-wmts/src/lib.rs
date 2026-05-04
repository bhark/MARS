//! WMTS 1.0.0 interface adapter. SPEC §13.

#![forbid(unsafe_code)]

use mars_runtime::RenderPlan;
use mars_types::ImageFormat;

#[derive(Debug, thiserror::Error)]
pub enum WmtsError {
    #[error("missing parameter: {0}")]
    MissingParam(&'static str),
    #[error("invalid parameter `{name}`: {reason}")]
    InvalidParam { name: &'static str, reason: String },
    #[error("not implemented: {what}")]
    NotImplemented { what: &'static str },
}

/// Parse a WMTS `GetTile` query into a `RenderPlan`. Phase 0 stub.
pub fn parse_get_tile(_query: &str) -> Result<RenderPlan, WmtsError> {
    Err(WmtsError::NotImplemented {
        what: "mars-wmts::parse_get_tile",
    })
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

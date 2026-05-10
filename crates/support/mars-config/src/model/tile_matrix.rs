use mars_types::CrsCode;
use serde::{Deserialize, Serialize};

/// Tile-matrix-set definition. SPEC §13.3.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileMatrixSet {
    /// CRS the matrix set is defined in.
    pub crs: CrsCode,
    /// Top-left corner in CRS units.
    pub top_left: [f64; 2],
    /// Tile pixel dimensions.
    pub tile_size: [u32; 2],
    /// Per-level definitions.
    pub levels: Vec<TileMatrixLevel>,
}

/// Single tile-matrix level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileMatrixLevel {
    /// Zoom-level index.
    pub id: u32,
    /// Scale denominator at this level.
    pub scale_denominator: f64,
    /// Width of the matrix in tiles. Required by OGC WMTS 1.0.0 (07-057r7
    /// §6.1) and surfaced verbatim in `Capabilities`. Defaults to 1 so the
    /// minimum-viable single-tile setup needs no boilerplate.
    #[serde(default = "one")]
    pub matrix_width: u32,
    /// Height of the matrix in tiles. See `matrix_width`.
    #[serde(default = "one")]
    pub matrix_height: u32,
}

fn one() -> u32 {
    1
}

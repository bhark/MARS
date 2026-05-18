use mars_types::{CrsCode, SourceCollectionId};
use serde::{Deserialize, Serialize};

/// Raster layer body. Carries the tile source binding plus per-layer
/// compositing knobs. The `locator` is opaque at this layer; the adapter
/// chosen by the bin interprets it (URL template, COG key, etc.).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RasterLayerSpec {
    /// Tile source binding.
    pub source: RasterSourceBinding,
    /// Per-layer opacity multiplier in `[0.0, 1.0]`. Defaults to `1.0`.
    #[serde(default = "default_raster_opacity")]
    pub opacity: f32,
}

/// Tile source binding for a raster layer. Maps the layer's collection id
/// onto a backend-side locator interpreted by whichever `RasterSource`
/// adapter the bin registers for that collection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RasterSourceBinding {
    /// Logical collection identifier the bin maps to a `RasterSource` impl.
    pub collection: SourceCollectionId,
    /// Opaque backend locator (URL template, COG key, etc.).
    pub locator: String,
    /// Native CRS of the source tiles.
    pub source_crs: CrsCode,
    /// Tile edge length in pixels. Defaults to 256 (slippy-map convention).
    #[serde(default = "default_raster_tile_size")]
    pub tile_size: u32,
    /// Inclusive maximum zoom level the source publishes.
    pub max_level: u32,
}

fn default_raster_opacity() -> f32 {
    1.0
}

fn default_raster_tile_size() -> u32 {
    256
}

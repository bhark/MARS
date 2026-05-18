//! Raster read port: tile-pyramid binding and one-tile read trait.
//!
//! Sits beside [`crate::Source`] so each adapter advertises the kind of
//! data it produces without one trait pretending to cover both.

use async_trait::async_trait;
use bytes::Bytes;
use mars_types::CrsCode;

use crate::{SourceCollectionId, SourceError};

/// Raster-side binding: identifies a pyramidal tile source and its native
/// addressing. Concrete adapters extend the locator semantics (XYZ URL
/// template, COG byte ranges, WMTS endpoint) - the port keeps the field
/// set minimal so the dispatch can remain backend-agnostic.
#[derive(Debug, Clone, PartialEq)]
pub struct RasterBinding {
    /// Logical collection name.
    pub collection: SourceCollectionId,
    /// Opaque backend-side locator (e.g. URL template, object-store prefix,
    /// COG key). Format is defined by the adapter.
    pub locator: String,
    /// Native source CRS of the underlying pyramid.
    pub source_crs: CrsCode,
    /// Native tile edge in pixels (typically 256 or 512). Adapters that
    /// produce variable-size tiles report their advertised default here.
    pub tile_size: u32,
    /// Maximum zoom level published by the source (inclusive).
    pub max_level: u32,
}

/// One raster tile pulled from a raster source. `bytes` carries the encoded
/// payload as the source delivered it (typically PNG / JPEG / WebP); the
/// renderer decodes lazily based on `content_type`.
#[derive(Debug, Clone)]
pub struct TileBytes {
    /// Encoded tile bytes.
    pub bytes: Bytes,
    /// IANA media type of the encoded payload, e.g. `"image/png"`.
    pub content_type: &'static str,
}

/// Read-side port for raster pyramids. Sits beside [`crate::Source`] so each
/// adapter advertises the kind of data it produces without one trait
/// pretending to cover both.
#[async_trait]
pub trait RasterSource: Send + Sync + 'static {
    /// Read one tile from the bound pyramid. Coordinates are
    /// `(zoom, x, y)` in the source's native tiling scheme; the caller is
    /// responsible for mapping the request CRS / TMS to the source pyramid.
    async fn read_tile(&self, binding: &RasterBinding, z: u32, x: u32, y: u32) -> Result<TileBytes, SourceError>;
}

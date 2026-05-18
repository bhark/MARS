//! manifest data-transfer object - the top-level wire envelope.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::MANIFEST_FORMAT_VERSION;
use crate::binding::BindingMetadata;
use crate::content::ArtifactEntry;
use crate::ids::{CrsCode, LayerId, SourceCollectionId};
use crate::spatial::{LayerSidecarEntry, PageEntry};

/// manifest data-transfer object.
///
/// substrate is `(binding × decimation_level × page)`. each render-time page
/// lookup is a binary search of `pages` (sorted by `(binding_id, level,
/// hilbert_range.0)`) plus a bounded linear scan for spatial-bbox hits.
/// `format_version` is bumped on incompatible changes to this struct;
/// readers reject anything other than the current value (exact-match only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// On-disk format version of this manifest envelope. Exact-match only.
    pub format_version: u32,
    pub version: u64,
    pub service: String,
    /// publication wall-clock time. SystemTime to avoid pulling chrono into
    /// the workspace; serde encodes as `{ secs_since_epoch, nanos_since_epoch }`.
    pub created_at: SystemTime,
    pub bindings: Vec<BindingMetadata>,
    /// page entries sorted by `(binding_id, level, hilbert_range.0)`; a render
    /// request resolves `(binding_id, level)` first, binary-searches into the
    /// matching slice, then linear-scans for spatial-bbox intersection.
    pub pages: Vec<PageEntry>,
    pub class_sidecars: Vec<LayerSidecarEntry>,
    pub label_sidecars: Vec<LayerSidecarEntry>,
    pub style_artifact: Option<ArtifactEntry>,
    /// optional bundled image-resources artifact, parallel to `style_artifact`.
    /// carries the bitmap pack referenced by `FillPaint::Image { name }` so
    /// runtime renderers resolve names without out-of-band coordination.
    /// `None` when no style references an image resource.
    #[serde(default)]
    pub image_artifact: Option<ArtifactEntry>,
    /// raster layer entries: metadata-only bindings (no published bytes).
    /// runtime reads these to dispatch tile fetches through the appropriate
    /// `RasterSource` adapter at render time. one entry per `kind: raster`
    /// layer in the source config; empty when no raster layers are declared.
    #[serde(default)]
    pub raster_layers: Vec<RasterLayerEntry>,
    /// opaque source-side cursor at which this manifest's state was captured
    /// (e.g. WAL position, change-stream token). snapshot compiles set this
    /// to `None`.
    pub source_version: Option<String>,
    /// monotonic counter cross-checked by readers as a sanity gate against
    /// out-of-order manifest pointer publishes.
    pub epoch: u64,
}

impl Manifest {
    /// build the smallest valid manifest: zero bindings, zero pages, zero
    /// sidecars. used by stubs and tests; production paths populate the
    /// collections from compiler output.
    #[must_use]
    pub fn empty(version: u64, service: impl Into<String>) -> Self {
        Self {
            format_version: MANIFEST_FORMAT_VERSION,
            version,
            service: service.into(),
            created_at: SystemTime::now(),
            bindings: Vec::new(),
            pages: Vec::new(),
            class_sidecars: Vec::new(),
            label_sidecars: Vec::new(),
            style_artifact: None,
            image_artifact: None,
            raster_layers: Vec::new(),
            source_version: None,
            epoch: 0,
        }
    }
}

/// raster layer manifest entry. metadata-only: the runtime fans this out to
/// the `RasterSource` adapter registered for `collection` and composites the
/// returned tiles into the render canvas. no bytes are published with the
/// manifest; the upstream tile endpoint (or COG, or PostGIS raster) is the
/// payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RasterLayerEntry {
    /// layer this entry binds. matches `Layer.name` in the source config.
    pub layer_id: LayerId,
    /// logical collection the runtime uses to pick a `RasterSource` impl.
    pub collection: SourceCollectionId,
    /// opaque backend locator (URL template, COG key, etc.). interpreted by
    /// the chosen adapter, not by core.
    pub locator: String,
    /// native CRS of the source tiles. the runtime first cut rejects requests
    /// whose plan CRS differs (reprojection is its own follow-up arc).
    pub source_crs: CrsCode,
    /// tile edge length in pixels (e.g. 256).
    pub tile_size: u32,
    /// inclusive maximum zoom level the source publishes.
    pub max_level: u32,
    /// per-layer opacity multiplier in `[0,1]`. clamped at draw time.
    pub opacity: f32,
}

#[cfg(test)]
mod tests;

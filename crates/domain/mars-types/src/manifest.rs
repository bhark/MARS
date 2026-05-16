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
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::bbox::Bbox;
    use crate::content::ContentHash;
    use crate::ids::{ArtifactKey, BindingId};
    use crate::spatial::{DecimationLevel, HilbertKey, LayerSidecarKind, PageId, PageKey};

    #[test]
    fn manifest_empty_roundtrip() {
        let m = Manifest::empty(1, "demo");
        assert_eq!(m.format_version, MANIFEST_FORMAT_VERSION);
        let s = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn manifest_roundtrip_populated() {
        let pk = PageKey {
            binding_id: BindingId::try_new("buildings").unwrap(),
            level: DecimationLevel::new(0),
            page_id: PageId::new(7),
        };
        let mut m = Manifest::empty(42, "demo");
        m.epoch = 1;
        m.bindings.push(BindingMetadata {
            binding_id: pk.binding_id.clone(),
            source_table: "public.buildings".to_owned(),
            native_crs: CrsCode::new("EPSG:25832"),
            feature_count_total: 100,
            combined_bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
            levels: vec![],
            page_membership_sidecar: None,
            cycles_since_reconcile: 0,
            last_reconcile_at: None,
        });
        m.pages.push(PageEntry {
            key: pk.clone(),
            content_hash: ContentHash::zero(),
            spatial_bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
            hilbert_range: (HilbertKey::min(), HilbertKey::max()),
            feature_count: 100,
            size_bytes: 4096,
        });
        m.class_sidecars.push(LayerSidecarEntry {
            layer_id: LayerId::new("buildings"),
            page_key: pk,
            content_hash: ContentHash::zero(),
            size_bytes: 256,
            kind: LayerSidecarKind::Class,
        });
        let s = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn manifest_roundtrip_with_image_artifact() {
        let mut m = Manifest::empty(7, "demo");
        m.image_artifact = Some(ArtifactEntry {
            key: ArtifactKey::new("images/pack.bin"),
            hash: ContentHash::zero(),
            size_bytes: 1234,
        });
        let s = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
        assert!(back.image_artifact.is_some());
    }

    #[test]
    fn manifest_roundtrip_with_raster_layers() {
        let mut m = Manifest::empty(8, "demo");
        m.raster_layers.push(RasterLayerEntry {
            layer_id: LayerId::new("osm"),
            collection: SourceCollectionId::new("osm"),
            locator: "https://tile.example/{z}/{x}/{y}.png".into(),
            source_crs: CrsCode::new("EPSG:3857"),
            tile_size: 256,
            max_level: 19,
            opacity: 1.0,
        });
        let s = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
        assert_eq!(back.raster_layers.len(), 1);
    }

    #[test]
    fn manifest_default_raster_layers_when_field_missing() {
        // raster_layers is `#[serde(default)]`: a body that omits it parses
        // to an empty vec rather than failing.
        let m = Manifest::empty(1, "x");
        let s = serde_json::to_string(&m).unwrap();
        let stripped = s.replacen(r#""raster_layers":[],"#, "", 1);
        let back: Manifest = serde_json::from_str(&stripped).expect("default applies");
        assert!(back.raster_layers.is_empty());
    }

    #[test]
    fn manifest_rejects_missing_format_version() {
        // no serde default: a manifest body lacking `format_version` is a
        // hard parse error, not a silent legacy floor.
        let valid = serde_json::to_string(&Manifest::empty(1, "x")).unwrap();
        assert!(serde_json::from_str::<Manifest>(&valid).is_ok());

        // strip the format_version field from the canonical body and confirm
        // serde refuses to default it.
        let stripped: String = valid.replacen(&format!(r#""format_version":{MANIFEST_FORMAT_VERSION},"#), "", 1);
        assert!(
            serde_json::from_str::<Manifest>(&stripped).is_err(),
            "missing format_version must be a parse error"
        );
    }
}

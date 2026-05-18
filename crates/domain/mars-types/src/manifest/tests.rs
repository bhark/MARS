#![allow(clippy::unwrap_used, clippy::expect_used)]

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

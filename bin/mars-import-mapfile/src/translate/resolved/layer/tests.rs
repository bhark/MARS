#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn group_path_collapses_flat_group_to_single_segment() {
    assert_eq!(normalize_group_path(None, Some("Basis")).as_deref(), Some("/Basis"));
}

#[test]
fn group_path_collapses_hierarchical_wms_group_path() {
    assert_eq!(
        normalize_group_path(Some("/Adresse/Bygning"), None).as_deref(),
        Some("/Adresse/Bygning")
    );
    // missing leading slash still produces a normalised path.
    assert_eq!(
        normalize_group_path(Some("Adresse/Bygning"), None).as_deref(),
        Some("/Adresse/Bygning")
    );
}

#[test]
fn group_path_wms_layer_group_wins_over_flat_group() {
    assert_eq!(
        normalize_group_path(Some("/A/B"), Some("Other")).as_deref(),
        Some("/A/B"),
    );
}

#[test]
fn group_path_drops_empty_segments_and_returns_none_when_blank() {
    assert_eq!(normalize_group_path(Some("///A// /B/"), None).as_deref(), Some("/A/B"));
    assert!(normalize_group_path(Some(""), None).is_none());
    assert!(normalize_group_path(Some("///"), None).is_none());
}

#[test]
fn advertise_only_layer_preserves_metadata_and_denies_get_map() {
    use super::super::super::layer::ParsedLayer;
    use super::super::super::layer_metadata::{LayerMetadata, MetadataUrlTriple, ParsedGating};

    let mut p = ParsedLayer {
        name: Some("Basis_root".into()),
        status_off: true,
        wms_layer_group: Some("/Basis".into()),
        ..ParsedLayer::default()
    };
    p.wms_metadata = LayerMetadata {
        title_override: Some("Basis kort".into()),
        abstract_override: Some("Foundation group".into()),
        keywords: vec!["basis".into()],
        metadata_urls: vec![MetadataUrlTriple {
            type_: "ISO19115:2003".into(),
            format: "text/xml".into(),
            href: "https://example.org/md.xml".into(),
        }],
        request_gating: ParsedGating {
            get_capabilities: Some(true),
            get_map: Some(false),
            ..ParsedGating::default()
        },
        ..LayerMetadata::default()
    };

    let r = resolve_layer(p, 1, &HashMap::new(), None, false).expect("advertise-only layer retained");
    assert_eq!(r.name, "Basis_root");
    assert_eq!(r.title.as_deref(), Some("Basis kort"));
    assert_eq!(r.abstract_.as_deref(), Some("Foundation group"));
    assert_eq!(r.group_path.as_deref(), Some("/Basis"));
    assert_eq!(r.ows.keywords, vec!["basis"]);
    assert_eq!(r.ows.metadata_urls.len(), 1);
    assert_eq!(r.ows.request_gating.get_map, Some(false));
    assert!(r.sources.is_empty(), "advertise-only must carry no sources");
    assert!(r.classes.is_empty(), "advertise-only must carry no classes");
    assert!(r.label.is_none(), "advertise-only must carry no label");
}

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::SystemTime;

use mars_types::{Bbox, ContentHash, HilbertKey, LayerId, MANIFEST_FORMAT_VERSION, PageId};

use super::*;

fn page(binding: &str, level: u8, hilbert_lo: u64, page_id: u64) -> PageEntry {
    PageEntry {
        key: PageKey {
            binding_id: BindingId::try_new(binding).unwrap(),
            level: DecimationLevel::new(level),
            page_id: PageId::new(page_id),
        },
        content_hash: ContentHash::zero(),
        spatial_bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
        hilbert_range: (HilbertKey::new(hilbert_lo), HilbertKey::new(hilbert_lo + 1)),
        feature_count: 0,
        size_bytes: 0,
    }
}

fn sidecar(layer: &str, page_key: PageKey, kind: LayerSidecarKind) -> LayerSidecarEntry {
    LayerSidecarEntry {
        layer_id: LayerId::new(layer),
        page_key,
        content_hash: ContentHash::zero(),
        size_bytes: 0,
        kind,
    }
}

fn manifest_with(pages: Vec<PageEntry>, bindings: Vec<BindingMetadata>) -> Manifest {
    Manifest {
        format_version: MANIFEST_FORMAT_VERSION,
        version: 1,
        service: "test".into(),
        created_at: SystemTime::UNIX_EPOCH,
        bindings,
        pages,
        class_sidecars: vec![],
        label_sidecars: vec![],
        style_artifact: None,
        image_artifact: None,
        raster_layers: Vec::new(),
        source_version: None,
        epoch: 0,
    }
}

#[test]
fn slices_are_contiguous_per_binding_level() {
    let pages = vec![
        page("a", 0, 0, 1),
        page("a", 0, 10, 2),
        page("a", 1, 0, 3),
        page("b", 0, 0, 4),
    ];
    let m = manifest_with(pages, vec![]);
    let idx = PageIndex::build(&m).unwrap();
    assert_eq!(idx.total_pages(), 4);
    assert_eq!(
        idx.page_slice(&m, &BindingId::try_new("a").unwrap(), DecimationLevel::new(0))
            .len(),
        2
    );
    assert_eq!(
        idx.page_slice(&m, &BindingId::try_new("a").unwrap(), DecimationLevel::new(1))
            .len(),
        1
    );
    assert_eq!(
        idx.page_slice(&m, &BindingId::try_new("b").unwrap(), DecimationLevel::new(0))
            .len(),
        1
    );
}

fn assert_unsorted_at(pages: Vec<PageEntry>, expected: usize) {
    let m = manifest_with(pages, vec![]);
    match PageIndex::build(&m) {
        Err(IndexError::PagesUnsorted { index }) => assert_eq!(index, expected),
        other => panic!("expected PagesUnsorted at {expected}, got {other:?}"),
    }
}

#[test]
fn rejects_unsorted_pages() {
    assert_unsorted_at(vec![page("b", 0, 0, 1), page("a", 0, 0, 2)], 1);
}

#[test]
fn rejects_unsorted_levels_within_binding() {
    assert_unsorted_at(vec![page("a", 1, 0, 1), page("a", 0, 0, 2)], 1);
}

#[test]
fn rejects_unsorted_hilbert_within_level() {
    assert_unsorted_at(vec![page("a", 0, 10, 1), page("a", 0, 0, 2)], 1);
}

#[test]
fn missing_binding_level_returns_empty_slice() {
    let pages = vec![page("a", 0, 0, 1)];
    let m = manifest_with(pages, vec![]);
    let idx = PageIndex::build(&m).unwrap();
    assert!(
        idx.page_slice(&m, &BindingId::try_new("a").unwrap(), DecimationLevel::new(7))
            .is_empty()
    );
    assert!(
        idx.page_slice(&m, &BindingId::try_new("missing").unwrap(), DecimationLevel::new(0))
            .is_empty()
    );
}

#[test]
fn orphan_class_sidecar_rejected() {
    let pages = vec![page("a", 0, 0, 1)];
    let mut m = manifest_with(pages, vec![]);
    let orphan_key = PageKey {
        binding_id: BindingId::try_new("ghost").unwrap(),
        level: DecimationLevel::new(0),
        page_id: PageId::new(99),
    };
    m.class_sidecars
        .push(sidecar("layer-a", orphan_key, LayerSidecarKind::Class));
    match PageIndex::build(&m) {
        Err(IndexError::OrphanSidecar { kind, .. }) => assert_eq!(kind, LayerSidecarKind::Class),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn orphan_sidecar_with_valid_binding_level_but_unknown_page_id_rejected() {
    // (binding, level) bucket exists but the specific page_id does not -
    // a coarse bucket-only check would let this stale sidecar survive.
    let pages = vec![page("a", 0, 0, 1)];
    let mut m = manifest_with(pages.clone(), vec![]);
    let stale = PageKey {
        binding_id: pages[0].key.binding_id.clone(),
        level: pages[0].key.level,
        page_id: PageId::new(pages[0].key.page_id.get() + 999),
    };
    m.class_sidecars
        .push(sidecar("layer-a", stale, LayerSidecarKind::Class));
    match PageIndex::build(&m) {
        Err(IndexError::OrphanSidecar { kind, .. }) => assert_eq!(kind, LayerSidecarKind::Class),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn duplicate_sidecar_rejected() {
    let pages = vec![page("a", 0, 0, 1)];
    let mut m = manifest_with(pages.clone(), vec![]);
    let key = pages[0].key.clone();
    m.label_sidecars
        .push(sidecar("layer-a", key.clone(), LayerSidecarKind::Label));
    m.label_sidecars.push(sidecar("layer-a", key, LayerSidecarKind::Label));
    match PageIndex::build(&m) {
        Err(IndexError::DuplicateSidecar { kind, .. }) => assert_eq!(kind, LayerSidecarKind::Label),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn empty_manifest_yields_empty_index() {
    let m = Manifest::empty(1, "svc");
    let idx = PageIndex::build(&m).unwrap();
    assert_eq!(idx.total_pages(), 0);
    assert_eq!(idx.binding_count(), 0);
}

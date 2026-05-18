#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::SystemTime;

use mars_config::{Class, LabelSurvival, ScaleWindow, SourceBinding};
use mars_style::Stylesheet;
use mars_types::{
    BindingMetadata, ContentHash, CrsCode, HilbertKey, LayerId, MANIFEST_FORMAT_VERSION, Manifest, PageId, PageKey,
};

use super::*;

fn level(level: u8, geometry_min_size_m: f64) -> LevelMetadata {
    LevelMetadata {
        level: DecimationLevel::new(level),
        vertex_tolerance_m: 0.0,
        geometry_min_size_m,
        label_min_priority: 0,
        page_count: 0,
        hilbert_range_table: vec![],
    }
}

fn page(binding: &str, lvl: u8, hilbert_lo: u64, page_id: u64, bbox: Bbox) -> PageEntry {
    PageEntry {
        key: PageKey {
            binding_id: BindingId::try_new(binding).unwrap(),
            level: DecimationLevel::new(lvl),
            page_id: PageId::new(page_id),
        },
        content_hash: ContentHash::zero(),
        spatial_bbox: bbox,
        hilbert_range: (HilbertKey::new(hilbert_lo), HilbertKey::new(hilbert_lo + 1)),
        feature_count: 0,
        size_bytes: 0,
    }
}

fn binding_meta(id: &str, levels: Vec<LevelMetadata>) -> BindingMetadata {
    BindingMetadata {
        binding_id: BindingId::try_new(id).unwrap(),
        source_table: id.to_owned(),
        native_crs: CrsCode::new("EPSG:25832"),
        feature_count_total: 0,
        combined_bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
        levels,
        page_membership_sidecar: None,
        cycles_since_reconcile: 0,
        last_reconcile_at: None,
    }
}

fn state_with(pages: Vec<PageEntry>, bindings: Vec<BindingMetadata>) -> RuntimeState {
    let manifest = Manifest {
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
    };
    let index = crate::PageIndex::build(&manifest).unwrap();
    RuntimeState {
        manifest,
        stylesheet: Stylesheet::default(),
        config: None,
        index,
    }
}

fn cfg_layer(name: &str, sources: Vec<SourceBinding>) -> Layer {
    Layer {
        name: LayerId::new(name),
        title: String::new(),
        abstract_: String::new(),
        kind: "polygon".into(),
        scale: None,
        group: None,
        bbox: None,
        sources,
        classes: Vec::<Class>::new(),
        label: None,
        label_survival: LabelSurvival::default(),
        raster: None,
        wms: Default::default(),
        ows: Default::default(),
        template: None,
    }
}

fn cfg_source(from: &str, scale: Option<ScaleWindow>) -> SourceBinding {
    SourceBinding {
        source: mars_config::SourceId::new("default"),
        scale,
        band: None,
        max_denom: None,
        filter: None,
        from: Some(from.into()),
        sql: None,
        uri: None,
        format: None,
        source_crs: None,
        geometry_column: "geom".into(),
        id_column: None,
        attributes: vec![],
        levels: None,
        page_size_target_bytes: None,
        reconcile_every_cycles: None,
        sidecar_size_warn_bytes: None,
        simplifier: None,
        on_missing_page: None,
        dsn: None,
    }
}

#[test]
fn pick_level_largest_under_threshold() {
    let levels = vec![level(0, 0.0), level(1, 5.0), level(2, 20.0), level(3, 100.0)];
    // pixel_size_m = denom × 0.00028; with denom = 357142, pixel ≈ 100m,
    // threshold = 50m → expect level 2 (20m).
    let chosen = pick_level(&levels, 100.0).unwrap();
    assert_eq!(chosen.get(), 2);
}

#[test]
fn pick_level_finest_when_all_too_coarse() {
    let levels = vec![level(0, 1.0), level(1, 5.0)];
    // threshold = 0.05m; nothing fits → fall back to finest (level 0).
    let chosen = pick_level(&levels, 0.1).unwrap();
    assert_eq!(chosen.get(), 0);
}

#[test]
fn pick_level_empty_returns_none() {
    assert!(pick_level(&[], 1.0).is_none());
}

#[test]
fn scale_window_inclusive_min_exclusive_max() {
    let w = ScaleWindow {
        min: Some(1000),
        max: Some(5000),
    };
    assert!(!scale_window_covers(Some(&w), 999));
    assert!(scale_window_covers(Some(&w), 1000));
    assert!(scale_window_covers(Some(&w), 4999));
    assert!(!scale_window_covers(Some(&w), 5000));
}

#[test]
fn scale_window_open_bounds() {
    let lo = ScaleWindow {
        min: Some(100),
        max: None,
    };
    assert!(!scale_window_covers(Some(&lo), 99));
    assert!(scale_window_covers(Some(&lo), 999_999));
    let hi = ScaleWindow {
        min: None,
        max: Some(100),
    };
    assert!(scale_window_covers(Some(&hi), 0));
    assert!(!scale_window_covers(Some(&hi), 100));
}

#[test]
fn resolve_pages_filters_by_bbox() {
    let pages = vec![
        page("a", 0, 0, 1, Bbox::new(0.0, 0.0, 10.0, 10.0)),
        page("a", 0, 1, 2, Bbox::new(20.0, 0.0, 30.0, 10.0)),
        page("a", 0, 2, 3, Bbox::new(5.0, 5.0, 15.0, 15.0)),
    ];
    let bindings = vec![binding_meta("a", vec![level(0, 0.0)])];
    let state = state_with(pages, bindings);
    let viewport = Bbox::new(0.0, 0.0, 12.0, 12.0);
    let hits = resolve_pages(
        &state,
        &BindingId::try_new("a").unwrap(),
        DecimationLevel::new(0),
        viewport,
    );
    assert_eq!(hits.len(), 2);
    // page ids 1 and 3 intersect.
    let ids: Vec<u64> = hits.iter().map(|p| p.key.page_id.get()).collect();
    assert!(ids.contains(&1));
    assert!(ids.contains(&3));
}

#[test]
fn resolve_pages_empty_for_unknown_binding() {
    let state = state_with(vec![], vec![]);
    let hits = resolve_pages(
        &state,
        &BindingId::try_new("ghost").unwrap(),
        DecimationLevel::new(0),
        Bbox::new(0.0, 0.0, 1.0, 1.0),
    );
    assert!(hits.is_empty());
}

#[test]
fn pick_binding_and_level_picks_first_covering_source() {
    let pages = vec![page("a", 0, 0, 1, Bbox::new(0.0, 0.0, 10.0, 10.0))];
    let bindings = vec![binding_meta("a", vec![level(0, 0.0)])];
    let state = state_with(pages, bindings);
    let layer = cfg_layer("layer-a", vec![cfg_source("a", None)]);
    let resolved = pick_binding_and_level(&layer, 1000, crate::OGC_STANDARDIZED_PIXEL_SIZE_M, &state).unwrap();
    assert_eq!(resolved.0.as_str(), "a");
    assert_eq!(resolved.1.get(), 0);
}

#[test]
fn pick_binding_and_level_skips_out_of_window_sources() {
    let pages = vec![
        page("hi", 0, 0, 1, Bbox::new(0.0, 0.0, 10.0, 10.0)),
        page("lo", 0, 0, 2, Bbox::new(0.0, 0.0, 10.0, 10.0)),
    ];
    let bindings = vec![
        binding_meta("hi", vec![level(0, 0.0)]),
        binding_meta("lo", vec![level(0, 0.0)]),
    ];
    let state = state_with(pages, bindings);
    let layer = cfg_layer(
        "layer-a",
        vec![
            cfg_source(
                "hi",
                Some(ScaleWindow {
                    min: None,
                    max: Some(2000),
                }),
            ),
            cfg_source(
                "lo",
                Some(ScaleWindow {
                    min: Some(2000),
                    max: None,
                }),
            ),
        ],
    );
    let at_high = pick_binding_and_level(&layer, 1000, crate::OGC_STANDARDIZED_PIXEL_SIZE_M, &state).unwrap();
    assert_eq!(at_high.0.as_str(), "hi");
    let at_low = pick_binding_and_level(&layer, 5000, crate::OGC_STANDARDIZED_PIXEL_SIZE_M, &state).unwrap();
    assert_eq!(at_low.0.as_str(), "lo");
}

#[test]
fn pick_binding_and_level_none_when_no_binding_in_manifest() {
    let state = state_with(vec![], vec![]);
    let layer = cfg_layer("layer-a", vec![cfg_source("ghost", None)]);
    assert!(pick_binding_and_level(&layer, 1000, crate::OGC_STANDARDIZED_PIXEL_SIZE_M, &state).is_none());
}

#[test]
fn pick_binding_and_level_selects_tier_by_denom() {
    let pages = vec![
        page("t0", 0, 0, 1, Bbox::new(0.0, 0.0, 10.0, 10.0)),
        page("t1", 0, 0, 2, Bbox::new(0.0, 0.0, 10.0, 10.0)),
        page("t2", 0, 0, 3, Bbox::new(0.0, 0.0, 10.0, 10.0)),
    ];
    let bindings = vec![
        binding_meta("t0", vec![level(0, 0.0)]),
        binding_meta("t1", vec![level(0, 0.0)]),
        binding_meta("t2", vec![level(0, 0.0)]),
    ];
    let state = state_with(pages, bindings);
    let layer = cfg_layer(
        "layer-a",
        vec![
            cfg_source(
                "t0",
                Some(ScaleWindow {
                    min: None,
                    max: Some(8_000),
                }),
            ),
            cfg_source(
                "t1",
                Some(ScaleWindow {
                    min: Some(8_000),
                    max: Some(10_000),
                }),
            ),
            cfg_source(
                "t2",
                Some(ScaleWindow {
                    min: Some(10_000),
                    max: Some(25_000),
                }),
            ),
        ],
    );
    let r0 = pick_binding_and_level(&layer, 5_000, crate::OGC_STANDARDIZED_PIXEL_SIZE_M, &state).unwrap();
    assert_eq!(r0.0.as_str(), "t0");
    let r1 = pick_binding_and_level(&layer, 8_500, crate::OGC_STANDARDIZED_PIXEL_SIZE_M, &state).unwrap();
    assert_eq!(r1.0.as_str(), "t1");
    let r2 = pick_binding_and_level(&layer, 12_000, crate::OGC_STANDARDIZED_PIXEL_SIZE_M, &state).unwrap();
    assert_eq!(r2.0.as_str(), "t2");
}

mod denom_from_plan {
    use super::denom_from_plan;

    #[test]
    fn ordinary_case() {
        // 256 m / (256 px * 1 m/px) = 1
        assert_eq!(denom_from_plan(256.0, 256, 1.0), 1);
        // 1000 m / (100 px * 0.5 m/px) = 20
        assert_eq!(denom_from_plan(1_000.0, 100, 0.5), 20);
    }

    #[test]
    fn zero_width_bbox_returns_max() {
        assert_eq!(denom_from_plan(0.0, 256, 1.0), u32::MAX);
    }

    #[test]
    fn negative_width_bbox_returns_max() {
        assert_eq!(denom_from_plan(-1.0, 256, 1.0), u32::MAX);
    }

    #[test]
    fn zero_width_px_returns_max() {
        assert_eq!(denom_from_plan(256.0, 0, 1.0), u32::MAX);
    }

    #[test]
    fn nonpositive_m_per_pixel_returns_max() {
        assert_eq!(denom_from_plan(256.0, 256, 0.0), u32::MAX);
        assert_eq!(denom_from_plan(256.0, 256, -1.0), u32::MAX);
    }

    #[test]
    fn infinite_inputs_return_max() {
        assert_eq!(denom_from_plan(f64::INFINITY, 256, 1.0), u32::MAX);
        assert_eq!(denom_from_plan(256.0, 256, f64::INFINITY), u32::MAX);
        assert_eq!(denom_from_plan(f64::NAN, 256, 1.0), u32::MAX);
    }

    #[test]
    fn denom_above_u32_max_clamps() {
        // bbox so large that the quotient exceeds u32::MAX.
        let huge = f64::from(u32::MAX) * 2.0;
        assert_eq!(denom_from_plan(huge, 1, 1.0), u32::MAX);
    }

    #[test]
    fn fractional_denom_truncates_toward_zero() {
        // 3.5 → 3
        assert_eq!(denom_from_plan(7.0, 2, 1.0), 3);
    }
}

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use mars_config::{
    Artifacts, Band, Cells, ClassStyle, Config, DEFAULT_PAGE_SIZE_TARGET_BYTES, DecimationLevelConfig, Interfaces,
    Scales, ServiceMeta, Source, SourceBinding,
};
use mars_types::{Bbox, BindingId, CrsCode, DecimationLevel, LayerId};
use std::collections::BTreeMap;

fn config_with(layers: Vec<mars_config::Layer>) -> Config {
    let mut size_per_band = BTreeMap::new();
    size_per_band.insert("hi".into(), "1024m".into());
    Config {
        service: ServiceMeta {
            name: "test".into(),
            ..Default::default()
        },
        sources: vec![Source {
            id: mars_config::SourceId::new("default"),
            native_crs: CrsCode::new("EPSG:25832"),
            backend: mars_config::SourceBackend::Postgis(mars_config::PostgisBackend {
                dsn: "memory://".into(),
                change_feed: None,
                pool: Default::default(),
                bootstrap: None,
            }),
        }],
        artifacts: Artifacts {
            store: mars_config::ArtifactStore {
                kind: "fs".into(),
                endpoint: None,
                bucket: None,
                prefix: None,
                path: Some("/tmp".into()),
                allow_http: false,
                ..Default::default()
            },
            cache: mars_config::ArtifactCache {
                path: "/tmp".into(),
                max_size: "1GiB".into(),
                eviction: "lru".into(),
                trust_path_hash: false,
            },
        },
        scales: Scales {
            bands: vec![Band {
                name: "hi".into(),
                max_denom: 25_000,
            }],
        },
        cells: Cells {
            grid: "regular".into(),
            origin: [0.0, 0.0],
            size_per_band,
            extent: Some(Bbox::new(0.0, 0.0, 1_000.0, 1_000.0)),
        },
        interfaces: Interfaces::default(),
        tile_matrix_sets: Default::default(),
        reprojection: Default::default(),
        styles: Default::default(),
        layers,
        observability: Default::default(),
        render: Default::default(),
        compiler: Default::default(),
    }
}

fn binding(from: &str) -> SourceBinding {
    SourceBinding {
        source: mars_config::SourceId::new("default"),
        scale: None,
        band: None,
        max_denom: None,
        filter: None,
        from: Some(from.into()),
        sql: None,
        uri: None,
        format: None,
        source_crs: None,
        geometry_column: "geom".into(),
        id_column: Some("id".into()),
        attributes: vec!["name".into()],
        levels: None,
        page_size_target_bytes: None,
        reconcile_every_cycles: None,
        sidecar_size_warn_bytes: None,
        simplifier: None,
        on_missing_page: None,
        dsn: None,
    }
}

fn sql_binding(sql: &str) -> SourceBinding {
    let mut b = binding("ignored");
    b.from = None;
    b.sql = Some(sql.into());
    b
}

fn layer(name: &str, sources: Vec<SourceBinding>) -> mars_config::Layer {
    mars_config::Layer {
        name: LayerId::new(name),
        title: String::new(),
        abstract_: String::new(),
        kind: "polygon".into(),
        scale: None,
        group: None,
        bbox: None,
        sources,
        classes: vec![mars_config::Class {
            name: "default".into(),
            title: String::new(),
            when: None,
            scale: None,
            style: ClassStyle::Inline(Box::default()),
            label: None,
        }],
        label: None,
        label_survival: mars_config::LabelSurvival::Independent,
        raster: None,
        wms: Default::default(),
        ows: Default::default(),
        template: None,
    }
}

#[test]
fn empty_config_yields_empty_plan() {
    let cfg = config_with(vec![]);
    let plan = build_bootstrap_plan(&cfg).unwrap();
    assert!(plan.bindings.is_empty());
}

#[test]
fn raster_layer_is_skipped_from_bindings_and_emitted_as_manifest_entry() {
    use mars_config::{RasterLayerSpec, RasterSourceBinding};
    let mut l = layer("r", vec![]);
    l.kind = "raster".into();
    l.classes = vec![];
    l.raster = Some(RasterLayerSpec {
        source: RasterSourceBinding {
            collection: mars_types::SourceCollectionId::new("osm"),
            locator: "https://tile.example/{z}/{x}/{y}.png".into(),
            source_crs: CrsCode::new("EPSG:3857"),
            tile_size: 256,
            max_level: 19,
        },
        opacity: 0.75,
    });
    let cfg = config_with(vec![l]);
    let plan = build_bootstrap_plan(&cfg).expect("raster-only config plans cleanly");
    assert!(plan.bindings.is_empty(), "no vector bindings expected");
    assert!(plan.layers.is_empty(), "no vector layer plans expected");
    assert_eq!(plan.raster_layers.len(), 1);
    let entry = &plan.raster_layers[0];
    assert_eq!(entry.layer_id.as_str(), "r");
    assert_eq!(entry.collection.as_str(), "osm");
    assert_eq!(entry.locator, "https://tile.example/{z}/{x}/{y}.png");
    assert_eq!(entry.source_crs.as_str(), "EPSG:3857");
    assert_eq!(entry.tile_size, 256);
    assert_eq!(entry.max_level, 19);
    assert!((entry.opacity - 0.75).abs() < f32::EPSILON);
}

#[test]
fn build_raster_layer_entries_skips_vector_layers() {
    let cfg = config_with(vec![layer("v", vec![binding("buildings")])]);
    let entries = build_raster_layer_entries(&cfg);
    assert!(entries.is_empty());
}

#[test]
fn single_binding_default_levels() {
    let cfg = config_with(vec![layer("a", vec![binding("buildings")])]);
    let plan = build_bootstrap_plan(&cfg).unwrap();
    assert_eq!(plan.bindings.len(), 1);
    let b = &plan.bindings[0];
    assert_eq!(b.binding_id.as_str(), "buildings");
    assert_eq!(b.source_table, "buildings");
    assert_eq!(b.geometry_field, "geom");
    assert_eq!(b.attributes, vec!["name".to_string()]);
    assert_eq!(b.native_crs.as_str(), "EPSG:25832");
    assert_eq!(b.levels.len(), 1);
    assert_eq!(b.levels[0].level, DecimationLevel::new(0));
    assert_eq!(b.page_size_target_bytes, DEFAULT_PAGE_SIZE_TARGET_BYTES);
}

#[test]
fn shared_binding_dedup_across_layers() {
    let cfg = config_with(vec![
        layer("a", vec![binding("parcels")]),
        layer("b", vec![binding("parcels")]),
    ]);
    let plan = build_bootstrap_plan(&cfg).unwrap();
    assert_eq!(plan.bindings.len(), 1);
    assert_eq!(plan.bindings[0].binding_id.as_str(), "parcels");
}

#[test]
fn two_bindings_three_levels_each() {
    let mut b1 = binding("a");
    b1.levels = Some(vec![
        DecimationLevelConfig {
            level: 0,
            vertex_tolerance_m: 0.0,
            geometry_min_size_m: 0.0,
            label_min_priority: 0,
        },
        DecimationLevelConfig {
            level: 1,
            vertex_tolerance_m: 1.0,
            geometry_min_size_m: 1.0,
            label_min_priority: 5,
        },
        DecimationLevelConfig {
            level: 2,
            vertex_tolerance_m: 4.0,
            geometry_min_size_m: 8.0,
            label_min_priority: 10,
        },
    ]);
    let b2 = binding("b");
    let cfg = config_with(vec![layer("l", vec![b1, b2])]);
    let plan = build_bootstrap_plan(&cfg).unwrap();
    assert_eq!(plan.bindings.len(), 2);
    assert_eq!(plan.bindings[0].levels.len(), 3);
    assert_eq!(plan.bindings[1].levels.len(), 1);
}

#[test]
fn layer_plan_parses_when_clauses_and_resolves_inline_style_ref() {
    let mut b = binding("buildings");
    b.attributes = vec!["kind".into()];
    let l = mars_config::Layer {
        name: LayerId::new("bygning"),
        title: String::new(),
        abstract_: String::new(),
        kind: "polygon".into(),
        scale: None,
        group: None,
        bbox: None,
        sources: vec![b],
        classes: vec![
            mars_config::Class {
                name: "main".into(),
                title: String::new(),
                when: Some("kind = 'main'".into()),
                scale: None,
                style: ClassStyle::Inline(Box::default()),
                label: None,
            },
            mars_config::Class {
                name: "default".into(),
                title: String::new(),
                when: None,
                scale: None,
                style: ClassStyle::Inline(Box::default()),
                label: None,
            },
        ],
        label: None,
        label_survival: mars_config::LabelSurvival::Independent,
        raster: None,
        wms: Default::default(),
        ows: Default::default(),
        template: None,
    };
    let cfg = config_with(vec![l]);
    let plan = build_bootstrap_plan(&cfg).unwrap();
    assert_eq!(plan.layers.len(), 1);
    let layer = &plan.layers[0];
    assert_eq!(layer.layer_id.as_str(), "bygning");
    assert_eq!(layer.binding_id.as_str(), "buildings");
    assert_eq!(layer.classes.len(), 2);
    assert!(layer.classes[0].when.is_some());
    assert!(layer.classes[1].when.is_none());
    assert_eq!(layer.classes[0].style_ref, "bygning__main");
    assert_eq!(layer.classes[1].style_ref, "bygning__default");
}

#[test]
fn layers_for_filters_to_target_binding() {
    let cfg = config_with(vec![
        layer("a", vec![binding("parcels")]),
        layer("b", vec![binding("buildings")]),
    ]);
    let plan = build_bootstrap_plan(&cfg).unwrap();
    let parcels = BindingId::try_new("parcels").unwrap();
    let collected: Vec<_> = plan
        .layers_for(&parcels)
        .map(|l| l.layer_id.as_str().to_string())
        .collect();
    assert_eq!(collected, vec!["a".to_string()]);
}

#[test]
fn rejects_conflicting_geometry_field() {
    let mut b1 = binding("parcels");
    let mut b2 = binding("parcels");
    b2.geometry_column = "shape".into();
    b1.geometry_column = "geom".into();
    let cfg = config_with(vec![layer("a", vec![b1]), layer("b", vec![b2])]);
    let err = build_bootstrap_plan(&cfg).unwrap_err();
    assert!(matches!(
        err,
        PlanError::ConflictingBinding {
            detail: "geometry_field",
            ..
        }
    ));
}

#[test]
fn rejects_conflicting_attributes() {
    let b1 = binding("parcels");
    let mut b2 = binding("parcels");
    b2.attributes = vec!["other".into()];
    let cfg = config_with(vec![layer("a", vec![b1]), layer("b", vec![b2])]);
    let err = build_bootstrap_plan(&cfg).unwrap_err();
    assert!(matches!(
        err,
        PlanError::ConflictingBinding {
            detail: "attributes",
            ..
        }
    ));
}

#[test]
fn rejects_conflicting_missing_page_policy() {
    let b1 = binding("parcels");
    let mut b2 = binding("parcels");
    b2.on_missing_page = Some(mars_config::MissingPagePolicy::Fail);
    let cfg = config_with(vec![layer("a", vec![b1]), layer("b", vec![b2])]);
    let err = build_bootstrap_plan(&cfg).unwrap_err();
    assert!(matches!(
        err,
        PlanError::ConflictingBinding {
            detail: "missing_page_policy",
            ..
        }
    ));
}

#[test]
fn rejects_conflicting_dsn() {
    let b1 = binding("parcels");
    let mut b2 = binding("parcels");
    b2.dsn = Some("postgresql://other/db".into());
    let cfg = config_with(vec![layer("a", vec![b1]), layer("b", vec![b2])]);
    let err = build_bootstrap_plan(&cfg).unwrap_err();
    assert!(matches!(err, PlanError::ConflictingBinding { detail: "dsn", .. }));
}

/// load -> validate -> propagate. exercises that per-level decimation
/// values declared on a binding survive the full pipeline into the
/// compiler's BindingPlan in declaration order. closes the gap noted
/// during the decimation audit where no test covered
/// the propagation end-to-end.
#[test]
fn binding_plan_carries_decimation_levels_in_order() {
    use std::path::Path;
    let mut b = binding("buildings");
    b.levels = Some(vec![
        DecimationLevelConfig {
            level: 0,
            vertex_tolerance_m: 0.0,
            geometry_min_size_m: 0.0,
            label_min_priority: 0,
        },
        DecimationLevelConfig {
            level: 1,
            vertex_tolerance_m: 2.5,
            geometry_min_size_m: 5.0,
            label_min_priority: 50,
        },
        DecimationLevelConfig {
            level: 2,
            vertex_tolerance_m: 10.0,
            geometry_min_size_m: 25.0,
            label_min_priority: 100,
        },
    ]);
    let mut cfg = config_with(vec![layer("l", vec![b])]);
    mars_config::validate(&mut cfg, Path::new(".")).expect("validate");
    let plan = build_bootstrap_plan(&cfg).expect("plan");
    assert_eq!(plan.bindings.len(), 1);
    let levels = &plan.bindings[0].levels;
    assert_eq!(levels.len(), 3);
    assert_eq!(levels[0].level, DecimationLevel::new(0));
    assert_eq!(levels[0].vertex_tolerance_m, 0.0);
    assert_eq!(levels[0].geometry_min_size_m, 0.0);
    assert_eq!(levels[0].label_min_priority, 0);
    assert_eq!(levels[1].level, DecimationLevel::new(1));
    assert_eq!(levels[1].vertex_tolerance_m, 2.5);
    assert_eq!(levels[1].geometry_min_size_m, 5.0);
    assert_eq!(levels[1].label_min_priority, 50);
    assert_eq!(levels[2].level, DecimationLevel::new(2));
    assert_eq!(levels[2].vertex_tolerance_m, 10.0);
    assert_eq!(levels[2].geometry_min_size_m, 25.0);
    assert_eq!(levels[2].label_min_priority, 100);
}

/// bands are routing rules, not substrate axes. two sources of
/// the same layer that resolve to the same binding must collapse to one
/// LayerPlan, otherwise rebuild emits duplicate sidecars per page.
#[test]
fn layer_with_two_sources_same_binding_dedupes_layer_plan() {
    let mut b1 = binding("vejmidte");
    b1.band = Some("hi".into());
    let mut b2 = binding("vejmidte");
    b2.band = Some("mid".into());
    let mut cfg = config_with(vec![layer("Vejmidte", vec![b1, b2])]);
    // band: mid must exist in scales.bands or config validation would
    // reject; the plan layer doesn't care, but keep the model coherent.
    cfg.scales.bands.push(Band {
        name: "mid".into(),
        max_denom: 250_000,
    });
    let plan = build_bootstrap_plan(&cfg).unwrap();
    assert_eq!(plan.bindings.len(), 1);
    assert_eq!(plan.layers.len(), 1, "expected one LayerPlan, got {:#?}", plan.layers);
    let id = BindingId::try_new("vejmidte").unwrap();
    assert_eq!(plan.layers_for(&id).count(), 1);
}

#[test]
fn three_tier_layer_produces_three_binding_plans_and_three_layer_plans() {
    let mut b0 = binding("a");
    b0.band = Some("hi".into());
    b0.max_denom = Some(8_000);
    let mut b1 = binding("b");
    b1.band = Some("hi".into());
    b1.max_denom = Some(10_000);
    let mut b2 = binding("c");
    b2.band = Some("hi".into());
    b2.max_denom = Some(25_000);
    let cfg = config_with(vec![layer("l", vec![b0, b1, b2])]);
    let plan = build_bootstrap_plan(&cfg).unwrap();
    assert_eq!(plan.bindings.len(), 3, "expected 3 distinct BindingPlans");
    assert_eq!(plan.layers.len(), 3, "expected 3 LayerPlans");
    for lp in &plan.layers {
        assert_eq!(lp.layer_id.as_str(), "l");
    }
}

#[test]
fn rejects_conflicting_layer_classes() {
    let b1 = binding("parcels");
    let b2 = binding("parcels");
    let l1 = layer("shared", vec![b1]);
    let mut l2 = layer("shared", vec![b2]);
    l2.classes = vec![mars_config::Class {
        name: "other".into(),
        title: String::new(),
        when: None,
        scale: None,
        style: ClassStyle::Inline(Box::default()),
        label: None,
    }];
    let cfg = config_with(vec![l1, l2]);
    let err = build_bootstrap_plan(&cfg).unwrap_err();
    assert!(
        matches!(err, PlanError::ConflictingLayer { detail: "classes", .. }),
        "unexpected error: {err:?}"
    );
}

/// sql: bindings (inline SELECT) land as parenthesised locators with a
/// content-derived BindingId so the adapter can splice them into
/// `FROM (...) AS s` and equal SELECTs across layers dedupe.
#[test]
fn sql_binding_yields_subquery_locator() {
    let cfg = config_with(vec![layer(
        "v",
        vec![sql_binding("SELECT id, geom, name FROM public.points WHERE active")],
    )]);
    let plan = build_bootstrap_plan(&cfg).unwrap();
    assert_eq!(plan.bindings.len(), 1);
    let b = &plan.bindings[0];
    assert!(
        b.source_table.starts_with("(SELECT") && b.source_table.ends_with(')'),
        "expected parenthesised SELECT, got {:?}",
        b.source_table
    );
    assert!(
        b.binding_id.as_str().starts_with("sql_"),
        "expected sql_-prefixed binding id, got {:?}",
        b.binding_id.as_str()
    );
}

#[test]
fn two_layers_share_sql_binding_dedupe() {
    let sql = "SELECT id, geom, name FROM public.points";
    let cfg = config_with(vec![
        layer("a", vec![sql_binding(sql)]),
        layer("b", vec![sql_binding(sql)]),
    ]);
    let plan = build_bootstrap_plan(&cfg).unwrap();
    assert_eq!(plan.bindings.len(), 1, "equal sql bodies must dedupe");
    assert_eq!(plan.layers.len(), 2);
}

#[test]
fn distinct_sql_bodies_produce_distinct_bindings() {
    let cfg = config_with(vec![layer(
        "v",
        vec![
            sql_binding("SELECT id, geom, name FROM public.a"),
            sql_binding("SELECT id, geom, name FROM public.b"),
        ],
    )]);
    let plan = build_bootstrap_plan(&cfg).unwrap();
    assert_eq!(plan.bindings.len(), 2);
    assert_ne!(plan.bindings[0].binding_id, plan.bindings[1].binding_id);
}

/// per-class LABEL plumbs through the plan layer with a deterministic
/// inline style_ref name so the page sidecar can address it by index.
/// classes without their own label leave ClassPlan.label = None so
/// emit_layer_sidecars can fall back to the layer-level label.
#[test]
fn class_label_lands_on_class_plan_with_scoped_style_ref() {
    use mars_config::{LabelStyleAttach, LayerLabel};
    let mut b = binding("vejnavne");
    b.attributes = vec!["kind".into(), "name".into()];
    let inline = LayerLabel {
        style: LabelStyleAttach::Inline(mars_style::LabelStyle {
            font_family: "DejaVu Sans".into(),
            font_size: 12.0.into(),
            fill: mars_style::Colour::rgb(0, 0, 0),
            halo: None,
            priority: 0,
            min_distance: 0.0,
            position: mars_style::AnchorPosition::default(),
            offset_px: (0.0, 0.0),
            angle: None,
            partials: false,
            force: false,
        }),
        text: "{name}".into(),
        placement: None,
    };
    let l = mars_config::Layer {
        name: LayerId::new("vejnavne"),
        title: String::new(),
        abstract_: String::new(),
        kind: "line".into(),
        scale: None,
        group: None,
        bbox: None,
        sources: vec![b],
        classes: vec![
            mars_config::Class {
                name: "major".into(),
                title: String::new(),
                when: Some("kind = 'major'".into()),
                scale: None,
                style: ClassStyle::Inline(Box::default()),
                label: Some(inline.clone()),
            },
            mars_config::Class {
                name: "default".into(),
                title: String::new(),
                when: None,
                scale: None,
                style: ClassStyle::Inline(Box::default()),
                label: None,
            },
        ],
        label: Some(inline),
        label_survival: mars_config::LabelSurvival::Independent,
        raster: None,
        wms: Default::default(),
        ows: Default::default(),
        template: None,
    };
    let cfg = config_with(vec![l]);
    let plan = build_bootstrap_plan(&cfg).unwrap();
    let layer = &plan.layers[0];
    let major = &layer.classes[0];
    let default = &layer.classes[1];
    let major_label = major.label.as_ref().expect("class label survives plan build");
    assert_eq!(major_label.style_ref, "vejnavne__major__label");
    assert!(default.label.is_none(), "class without LABEL stays empty");
    assert_eq!(
        layer.label.as_ref().expect("layer label still present").style_ref,
        "vejnavne__label"
    );
}

/// label style: { name: ... } referencing a non-existent style must be a
/// typed plan-build error rather than silently substituting defaults. the
/// config validator already rejects this; constructing the Config directly
/// bypasses validation so the plan builder's own guard is exercised.
#[test]
fn unknown_label_style_ref_is_a_plan_error() {
    use mars_config::{LabelStyleAttach, LayerLabel};
    let mut l = layer("vejnavne", vec![binding("vejnavne")]);
    l.label = Some(LayerLabel {
        style: LabelStyleAttach::Ref { name: "missing".into() },
        text: "{name}".into(),
        placement: None,
    });
    let cfg = config_with(vec![l]);
    let err = build_bootstrap_plan(&cfg).unwrap_err();
    match err {
        PlanError::UnknownLabelStyleRef { layer, name } => {
            assert_eq!(layer.as_str(), "vejnavne");
            assert_eq!(name, "missing");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

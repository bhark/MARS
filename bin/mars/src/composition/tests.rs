#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use mars_config::{
    ArtifactCache, ArtifactStore, Artifacts, ChangeFeed as CfgChangeFeed, Class, ClassStyle, Compiler, Config,
    Interfaces, Layer, PostgisBackend, Scales, ServiceMeta, Source as CfgSource, SourceBackend as CfgBackend,
    SourceBinding as CfgBinding, SourceId, model::Band,
};
use mars_types::{CrsCode, LayerId};

fn cfg_with_layers(layers: Vec<Layer>) -> Config {
    Config {
        service: ServiceMeta {
            name: "svc".into(),
            ..Default::default()
        },
        sources: vec![CfgSource {
            id: SourceId::new("default"),
            native_crs: CrsCode::new("EPSG:25832"),
            backend: CfgBackend::Postgis(PostgisBackend {
                dsn: "postgres://x".into(),
                change_feed: Some(CfgChangeFeed {
                    kind: "pgoutput".into(),
                    publication: Some("pub".into()),
                    slot: Some("slot".into()),
                    poll_interval: None,
                }),
                pool: Default::default(),
                bootstrap: None,
            }),
        }],
        artifacts: Artifacts {
            store: ArtifactStore {
                kind: "fs".into(),
                endpoint: None,
                bucket: None,
                prefix: None,
                path: Some("/tmp".into()),
                allow_http: false,
                ..Default::default()
            },
            cache: ArtifactCache {
                path: "/tmp".into(),
                max_size: "1MiB".into(),
                trust_path_hash: false,
            },
        },
        scales: Scales {
            bands: vec![
                Band {
                    name: "hi".into(),
                    max_denom: 25_000,
                },
                Band {
                    name: "lo".into(),
                    max_denom: 100_000,
                },
            ],
        },
        interfaces: Interfaces::default(),
        tile_matrix_sets: Default::default(),
        reprojection: Default::default(),
        styles: Default::default(),
        layers,
        observability: Default::default(),
        render: Default::default(),
        compiler: Compiler::default(),
    }
}

fn layer(name: &str, bindings: Vec<(&str, &str)>) -> Layer {
    Layer {
        name: LayerId::new(name),
        title: String::new(),
        abstract_: String::new(),
        kind: "polygon".into(),
        scale: None,
        group: None,
        bbox: None,
        sources: bindings
            .into_iter()
            .map(|(from, geom)| CfgBinding {
                source: SourceId::new("default"),
                kind: mars_config::BindingKind::PostgisTable {
                    from: from.into(),
                    geometry_column: geom.into(),
                    dsn: None,
                },
                scale: None,
                band: None,
                max_denom: None,
                filter: None,
                id_column: None,
                attributes: vec![],
                levels: None,
                page_size_target_bytes: None,
                reconcile_every_cycles: None,
                sidecar_size_warn_bytes: None,
                simplifier: None,
                on_missing_page: None,
            })
            .collect(),
        classes: vec![Class {
            name: "c".into(),
            title: String::new(),
            when: None,
            scale: None,
            style: ClassStyle::Ref { name: "x".into() },
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
fn build_replication_topology_dedups_shared_table() {
    let cfg = cfg_with_layers(vec![
        layer("a", vec![("public.roads", "geom")]),
        layer("b", vec![("public.roads", "geom")]),
        layer("c", vec![("buildings", "shape")]),
    ]);
    let topo = build_replication_topology(&cfg).unwrap();
    assert_eq!(topo.collections.len(), 2);
    assert!(
        topo.collections
            .iter()
            .any(|c| c.schema == "public" && c.table == "roads")
    );
    // unqualified `from` defaults to public
    assert!(
        topo.collections
            .iter()
            .any(|c| c.schema == "public" && c.table == "buildings")
    );
}

#[test]
fn build_replication_topology_carries_geometry_column_per_collection() {
    let cfg = cfg_with_layers(vec![layer("a", vec![("public.roads", "the_geom")])]);
    let topo = build_replication_topology(&cfg).unwrap();
    assert_eq!(topo.collections[0].geometry_column, "the_geom");
    assert_eq!(topo.collections[0].id_column, "id");
    assert_eq!(topo.collections[0].collection.as_str(), "public.roads");
}

#[test]
fn build_replication_topology_rejects_conflicting_relation_geometry_column() {
    let cfg = cfg_with_layers(vec![
        layer("a", vec![("public.roads", "geom")]),
        layer("b", vec![("public.roads", "shape")]),
    ]);
    let err = build_replication_topology(&cfg).unwrap_err().to_string();
    assert!(err.contains("conflicting geometry_column"), "{err}");
}

#[test]
fn build_replication_topology_rejects_conflicting_relation_id_column() {
    let mut cfg = cfg_with_layers(vec![
        layer("a", vec![("public.roads", "geom")]),
        layer("b", vec![("public.roads", "geom")]),
    ]);
    cfg.layers[1].sources[0].id_column = Some("gid".into());
    let err = build_replication_topology(&cfg).unwrap_err().to_string();
    assert!(err.contains("conflicting id_column"), "{err}");
}

#[test]
fn build_replication_topology_rejects_relation_source_aliases() {
    let cfg = cfg_with_layers(vec![
        layer("a", vec![("roads", "geom")]),
        layer("b", vec![("public.roads", "geom")]),
    ]);
    let err = build_replication_topology(&cfg).unwrap_err().to_string();
    assert!(err.contains("multiple source names"), "{err}");
}

#[test]
fn build_replication_topology_rejects_empty_layer_set() {
    let cfg = cfg_with_layers(vec![]);
    assert!(build_replication_topology(&cfg).is_err());
}

#[test]
fn build_replication_topology_three_tier_layer_yields_three_collections() {
    let cfg = cfg_with_layers(vec![Layer {
        name: LayerId::new("bygning"),
        title: String::new(),
        abstract_: String::new(),
        kind: "polygon".into(),
        scale: None,
        group: None,
        bbox: None,
        sources: vec![
            CfgBinding {
                source: mars_config::SourceId::new("default"),
                kind: mars_config::BindingKind::PostgisTable {
                    from: "public.bygning".into(),
                    geometry_column: "geom".into(),
                    dsn: None,
                },
                scale: None,
                band: Some("hi".into()),
                max_denom: Some(8_000),
                filter: None,
                id_column: None,
                attributes: vec![],
                levels: None,
                page_size_target_bytes: None,
                reconcile_every_cycles: None,
                sidecar_size_warn_bytes: None,
                simplifier: None,
                on_missing_page: None,
            },
            CfgBinding {
                source: mars_config::SourceId::new("default"),
                kind: mars_config::BindingKind::PostgisTable {
                    from: "public.bygning_1m".into(),
                    geometry_column: "geom".into(),
                    dsn: None,
                },
                scale: None,
                band: Some("hi".into()),
                max_denom: Some(10_000),
                filter: None,
                id_column: None,
                attributes: vec![],
                levels: None,
                page_size_target_bytes: None,
                reconcile_every_cycles: None,
                sidecar_size_warn_bytes: None,
                simplifier: None,
                on_missing_page: None,
            },
            CfgBinding {
                source: mars_config::SourceId::new("default"),
                kind: mars_config::BindingKind::PostgisTable {
                    from: "public.bygning_2m".into(),
                    geometry_column: "geom".into(),
                    dsn: None,
                },
                scale: None,
                band: Some("hi".into()),
                max_denom: Some(25_000),
                filter: None,
                id_column: None,
                attributes: vec![],
                levels: None,
                page_size_target_bytes: None,
                reconcile_every_cycles: None,
                sidecar_size_warn_bytes: None,
                simplifier: None,
                on_missing_page: None,
            },
        ],
        classes: vec![Class {
            name: "c".into(),
            title: String::new(),
            when: None,
            scale: None,
            style: ClassStyle::Ref { name: "x".into() },
            label: None,
        }],
        label: None,
        label_survival: mars_config::LabelSurvival::Independent,
        raster: None,
        wms: Default::default(),
        ows: Default::default(),
        template: None,
    }]);
    let topo = build_replication_topology(&cfg).unwrap();
    assert_eq!(topo.collections.len(), 3);
    assert!(topo.collections.iter().any(|c| c.table == "bygning"));
    assert!(topo.collections.iter().any(|c| c.table == "bygning_1m"));
    assert!(topo.collections.iter().any(|c| c.table == "bygning_2m"));
}

#[test]
fn validate_change_feed_config_accepts_pgoutput_with_slot_and_publication() {
    let cfg = cfg_with_layers(vec![]);
    validate_change_feed_config(&cfg).unwrap();
}

/// Mutable accessor for the test fixture's postgis backend. tests reach
/// in to flip change_feed knobs; the assertion documents the seeded shape.
fn pg_mut(cfg: &mut Config) -> &mut PostgisBackend {
    match &mut cfg.sources[0].backend {
        CfgBackend::Postgis(pg) => pg,
        _ => panic!("test fixture must have postgis as first source"),
    }
}

#[test]
fn validate_change_feed_config_rejects_missing_block() {
    let mut cfg = cfg_with_layers(vec![]);
    pg_mut(&mut cfg).change_feed = None;
    assert!(validate_change_feed_config(&cfg).is_err());
}

#[test]
fn validate_change_feed_config_rejects_unknown_type() {
    let mut cfg = cfg_with_layers(vec![]);
    pg_mut(&mut cfg).change_feed.as_mut().unwrap().kind = "polling".into();
    let err = validate_change_feed_config(&cfg).unwrap_err().to_string();
    assert!(err.contains("only 'pgoutput'"), "{err}");
}

#[test]
fn validate_change_feed_config_rejects_missing_publication() {
    let mut cfg = cfg_with_layers(vec![]);
    pg_mut(&mut cfg).change_feed.as_mut().unwrap().publication = None;
    assert!(validate_change_feed_config(&cfg).is_err());
}

#[test]
fn validate_change_feed_config_rejects_missing_slot() {
    let mut cfg = cfg_with_layers(vec![]);
    pg_mut(&mut cfg).change_feed.as_mut().unwrap().slot = Some(String::new());
    assert!(validate_change_feed_config(&cfg).is_err());
}

// raster source wiring --------------------------------------------------

fn raster_layer(name: &str, collection: &str) -> Layer {
    use mars_config::{RasterLayerSpec, RasterSourceBinding};
    Layer {
        name: LayerId::new(name),
        title: String::new(),
        abstract_: String::new(),
        kind: "raster".into(),
        scale: None,
        group: None,
        bbox: None,
        sources: vec![],
        classes: vec![],
        label: None,
        label_survival: mars_config::LabelSurvival::Independent,
        raster: Some(RasterLayerSpec {
            source: RasterSourceBinding {
                collection: SourceCollectionId::new(collection),
                locator: "https://tile.example/{z}/{x}/{y}.png".into(),
                source_crs: CrsCode::new("EPSG:3857"),
                tile_size: 256,
                max_level: 19,
            },
            opacity: 1.0,
        }),
        wms: Default::default(),
        ows: Default::default(),
        template: None,
    }
}

#[test]
fn build_raster_sources_empty_when_no_raster_layers() {
    let cfg = cfg_with_layers(vec![]);
    let sources = build_raster_sources(&cfg).unwrap();
    assert!(sources.is_empty());
}

#[test]
fn build_raster_sources_keys_by_collection() {
    let cfg = cfg_with_layers(vec![raster_layer("a", "osm"), raster_layer("b", "stamen")]);
    let sources = build_raster_sources(&cfg).unwrap();
    assert_eq!(sources.len(), 2);
    assert!(sources.get(&SourceCollectionId::new("osm")).is_some());
    assert!(sources.get(&SourceCollectionId::new("stamen")).is_some());
}

#[test]
fn build_raster_sources_dedupes_shared_collection_across_layers() {
    // two layers may share the same upstream collection (e.g. an opacity
    // overlay of the same OSM pyramid); the registry should still carry
    // exactly one entry per collection id.
    let cfg = cfg_with_layers(vec![
        raster_layer("a", "osm"),
        raster_layer("b", "osm"),
        raster_layer("c", "stamen"),
    ]);
    let sources = build_raster_sources(&cfg).unwrap();
    assert_eq!(sources.len(), 2);
}

//! Composition helpers: turn parsed configuration into adapter wiring.
//!
//! Lives in the bin crate because adapter-shaped types (here:
//! `mars_source_postgres::ReplicationTopology`) are concrete; library crates
//! must not name them directly per the hexagonal-architecture rules.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use mars_config::Config;
use mars_source::RasterSource;
use mars_source_postgres::{CollectionTopology, ReplicationTopology, SourceCollectionId};
use mars_source_xyz::XyzRasterSource;

/// Build the replication topology from configuration. Deduplicates source
/// bindings on `(schema, table)` so the same physical table appearing in
/// multiple layers maps to a single replication entry.
pub(crate) fn build_replication_topology(cfg: &Config) -> Result<ReplicationTopology> {
    let mut seen: BTreeMap<(String, String), CollectionTopology> = BTreeMap::new();
    for layer in &cfg.layers {
        for binding in &layer.sources {
            // raw-SQL bindings are snapshot-only and don't participate in the
            // logical-replication topology; the change-feed cannot route
            // pgoutput events back to an inline view. config validation
            // already accepts the binding; skip it here.
            let Some((schema, table)) = binding.schema_table() else {
                continue;
            };
            let from = match binding.from.as_deref() {
                Some(from) => from,
                None => continue,
            };
            let id_column = binding.id_column.as_deref().unwrap_or("id");
            let key = (schema.to_string(), table.to_string());
            if let Some(existing) = seen.get(&key) {
                if existing.geometry_column != binding.geometry_column {
                    return Err(anyhow!(
                        "source relation {schema}.{table} has conflicting geometry_column values: {:?} vs {:?}",
                        existing.geometry_column,
                        binding.geometry_column
                    ));
                }
                if existing.id_column != id_column {
                    return Err(anyhow!(
                        "source relation {schema}.{table} has conflicting id_column values: {:?} vs {:?}",
                        existing.id_column,
                        id_column
                    ));
                }
                if existing.collection.as_str() != from {
                    return Err(anyhow!(
                        "source relation {schema}.{table} is declared with multiple source names: {:?} vs {:?}",
                        existing.collection.as_str(),
                        from
                    ));
                }
                continue;
            }
            seen.insert(
                key,
                CollectionTopology {
                    collection: SourceCollectionId::new(from.to_owned()),
                    schema: schema.to_string(),
                    table: table.to_string(),
                    geometry_column: binding.geometry_column.clone(),
                    id_column: id_column.to_string(),
                },
            );
        }
    }
    let collections: Vec<CollectionTopology> = seen.into_values().collect();
    if collections.is_empty() {
        return Err(anyhow!(
            "no source bindings found; compiler mode needs at least one layer.sources entry"
        ));
    }

    Ok(ReplicationTopology { collections })
}

/// Validate the change-feed configuration block for compiler mode. Runtime
/// mode never reads it, so this is only called from the compiler boot path.
pub(crate) fn validate_change_feed_config(cfg: &Config) -> Result<()> {
    let feed = cfg
        .source
        .change_feed
        .as_ref()
        .ok_or_else(|| anyhow!("source.change_feed is required for compiler / all-in-one mode"))?;
    match feed.kind.as_str() {
        "pgoutput" => {
            let publication = feed.publication.as_deref().unwrap_or("");
            let slot = feed.slot.as_deref().unwrap_or("");
            if publication.is_empty() {
                return Err(anyhow!("source.change_feed.publication is required for type=pgoutput"));
            }
            if slot.is_empty() {
                return Err(anyhow!("source.change_feed.slot is required for type=pgoutput"));
            }
            Ok(())
        }
        other => Err(anyhow!(
            "source.change_feed.type='{other}' unsupported; only 'pgoutput' is wired"
        )),
    }
}

/// Build the per-collection raster source registry the runtime hands to its
/// raster render path. One shared [`reqwest::Client`] backs every XYZ
/// collection (connection pooling per upstream host is reqwest's job). The
/// returned map is keyed by `RasterLayerEntry.collection`; an empty config
/// (no raster layers) yields an empty map and zero adapter allocations.
pub(crate) fn build_raster_sources(cfg: &Config) -> Result<HashMap<SourceCollectionId, Arc<dyn RasterSource>>> {
    let mut out: HashMap<SourceCollectionId, Arc<dyn RasterSource>> = HashMap::new();
    let mut xyz_client: Option<Arc<dyn RasterSource>> = None;
    for layer in &cfg.layers {
        let Some(raster) = layer.raster.as_ref() else {
            continue;
        };
        let collection = SourceCollectionId::new(raster.source.collection.as_str().to_owned());
        if out.contains_key(&collection) {
            // multiple layers may share the same collection (different
            // opacities, same upstream tile pyramid); first registration wins.
            continue;
        }
        let source = xyz_client
            .get_or_insert_with(|| Arc::new(XyzRasterSource::new(reqwest::Client::new())) as Arc<dyn RasterSource>)
            .clone();
        out.insert(collection, source);
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_config::{
        ArtifactCache, ArtifactStore, Artifacts, Cells, ChangeFeed as CfgChangeFeed, Class, ClassStyle, Compiler,
        Config, Interfaces, Layer, Scales, ServiceMeta, Source as CfgSource, SourceBinding as CfgBinding, model::Band,
    };
    use mars_types::{Bbox, CrsCode, LayerId};
    use std::collections::BTreeMap;

    fn cfg_with_layers(layers: Vec<Layer>) -> Config {
        let mut size_per_band = BTreeMap::new();
        size_per_band.insert("hi".to_string(), "4096m".to_string());
        size_per_band.insert("lo".to_string(), "16384m".to_string());

        Config {
            service: ServiceMeta {
                name: "svc".into(),
                ..Default::default()
            },
            source: CfgSource {
                kind: "postgis".into(),
                dsn: "postgres://x".into(),
                native_crs: CrsCode::new("EPSG:25832"),
                change_feed: Some(CfgChangeFeed {
                    kind: "pgoutput".into(),
                    publication: Some("pub".into()),
                    slot: Some("slot".into()),
                    poll_interval: None,
                }),
                pool: Default::default(),
            },
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
                    eviction: "lru".into(),
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
            cells: Cells {
                grid: "regular".into(),
                origin: [0.0, 0.0],
                size_per_band,
                extent: Some(Bbox::new(0.0, 0.0, 1.0, 1.0)),
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
            enable_get_feature_info: false,
            bbox: None,
            sources: bindings
                .into_iter()
                .map(|(from, geom)| CfgBinding {
                    scale: None,
                    band: None,
                    max_denom: None,
                    filter: None,
                    from: Some(from.into()),
                    sql: None,
                    geometry_column: geom.into(),
                    id_column: None,
                    attributes: vec![],
                    levels: None,
                    page_size_target_bytes: None,
                    reconcile_every_cycles: None,
                    sidecar_size_warn_bytes: None,
                    simplifier: None,
                })
                .collect(),
            classes: vec![Class {
                name: "c".into(),
                title: String::new(),
                when: None,
                scale: None,
                style: ClassStyle::Ref { name: "x".into() },
            }],
            label: None,
            label_survival: mars_config::LabelSurvival::Independent,
            raster: None,
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
            enable_get_feature_info: false,
            bbox: None,
            sources: vec![
                CfgBinding {
                    scale: None,
                    band: Some("hi".into()),
                    max_denom: Some(8_000),
                    filter: None,
                    from: Some("public.bygning".into()),
                    sql: None,
                    geometry_column: "geom".into(),
                    id_column: None,
                    attributes: vec![],
                    levels: None,
                    page_size_target_bytes: None,
                    reconcile_every_cycles: None,
                    sidecar_size_warn_bytes: None,
                    simplifier: None,
                },
                CfgBinding {
                    scale: None,
                    band: Some("hi".into()),
                    max_denom: Some(10_000),
                    filter: None,
                    from: Some("public.bygning_1m".into()),
                    sql: None,
                    geometry_column: "geom".into(),
                    id_column: None,
                    attributes: vec![],
                    levels: None,
                    page_size_target_bytes: None,
                    reconcile_every_cycles: None,
                    sidecar_size_warn_bytes: None,
                    simplifier: None,
                },
                CfgBinding {
                    scale: None,
                    band: Some("hi".into()),
                    max_denom: Some(25_000),
                    filter: None,
                    from: Some("public.bygning_2m".into()),
                    sql: None,
                    geometry_column: "geom".into(),
                    id_column: None,
                    attributes: vec![],
                    levels: None,
                    page_size_target_bytes: None,
                    reconcile_every_cycles: None,
                    sidecar_size_warn_bytes: None,
                    simplifier: None,
                },
            ],
            classes: vec![Class {
                name: "c".into(),
                title: String::new(),
                when: None,
                scale: None,
                style: ClassStyle::Ref { name: "x".into() },
            }],
            label: None,
            label_survival: mars_config::LabelSurvival::Independent,
            raster: None,
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

    #[test]
    fn validate_change_feed_config_rejects_missing_block() {
        let mut cfg = cfg_with_layers(vec![]);
        cfg.source.change_feed = None;
        assert!(validate_change_feed_config(&cfg).is_err());
    }

    #[test]
    fn validate_change_feed_config_rejects_unknown_type() {
        let mut cfg = cfg_with_layers(vec![]);
        cfg.source.change_feed.as_mut().unwrap().kind = "polling".into();
        let err = validate_change_feed_config(&cfg).unwrap_err().to_string();
        assert!(err.contains("only 'pgoutput'"), "{err}");
    }

    #[test]
    fn validate_change_feed_config_rejects_missing_publication() {
        let mut cfg = cfg_with_layers(vec![]);
        cfg.source.change_feed.as_mut().unwrap().publication = None;
        assert!(validate_change_feed_config(&cfg).is_err());
    }

    #[test]
    fn validate_change_feed_config_rejects_missing_slot() {
        let mut cfg = cfg_with_layers(vec![]);
        cfg.source.change_feed.as_mut().unwrap().slot = Some(String::new());
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
            enable_get_feature_info: false,
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
        assert!(sources.contains_key(&SourceCollectionId::new("osm")));
        assert!(sources.contains_key(&SourceCollectionId::new("stamen")));
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
}

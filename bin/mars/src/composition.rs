//! Composition helpers: turn parsed configuration into adapter wiring.
//!
//! Lives in the bin crate because adapter-shaped types (here:
//! `mars_source_postgres::ReplicationTopology`) are concrete; library crates
//! must not name them directly per the hexagonal-architecture rules.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow};
use mars_config::Config;
use mars_grid::BandConfig;
use mars_source_postgres::{CollectionTopology, ReplicationTopology, SourceCollectionId};
use mars_types::ScaleBand;

/// Hard ceiling on cells emitted per row by the change-feed translator.
/// Mirrors the planner's per-band limit; both ends keep the runtime cost of a
/// single misbehaving geometry bounded.
const MAX_CELLS_PER_ROW: usize = 4096;

/// Build the replication topology from configuration. Deduplicates source
/// bindings on `(schema, table, geometry_column)` so the same physical table
/// appearing in multiple layers maps to a single replication entry.
pub(crate) fn build_replication_topology(cfg: &Config) -> Result<ReplicationTopology> {
    let origin = (cfg.cells.origin[0], cfg.cells.origin[1]);
    let cell_sizes = cfg.cells.size_per_band_m().context("resolve cells.size_per_band")?;

    let mut bands = Vec::with_capacity(cfg.scales.bands.len());
    for band in &cfg.scales.bands {
        let cell_size = cell_sizes
            .get(&band.name)
            .copied()
            .ok_or_else(|| anyhow!("cells.size_per_band missing entry for band '{}'", band.name))?;
        bands.push(BandConfig {
            name: ScaleBand::new(band.name.as_str()),
            max_denom: u32::try_from(band.max_denom).unwrap_or(u32::MAX),
            origin,
            cell_size,
        });
    }

    let mut seen: BTreeMap<(String, String, String), CollectionTopology> = BTreeMap::new();
    for layer in &cfg.layers {
        for binding in &layer.sources {
            let (schema, table) = binding.schema_table();
            let key = (schema.to_string(), table.to_string(), binding.geometry_column.clone());
            seen.entry(key).or_insert_with(|| CollectionTopology {
                collection: SourceCollectionId::new(binding.from.clone()),
                schema: schema.to_string(),
                table: table.to_string(),
                geometry_column: binding.geometry_column.clone(),
            });
        }
    }
    let collections: Vec<CollectionTopology> = seen.into_values().collect();
    if collections.is_empty() {
        return Err(anyhow!(
            "no source bindings found; compiler mode needs at least one layer.sources entry"
        ));
    }

    Ok(ReplicationTopology {
        collections,
        bands,
        max_cells_per_row: MAX_CELLS_PER_ROW,
    })
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
                    from: from.into(),
                    geometry_column: geom.into(),
                    id_column: None,
                    attributes: vec![],
                })
                .collect(),
            classes: vec![Class {
                name: "c".into(),
                title: String::new(),
                when: None,
                style: ClassStyle::Ref { name: "x".into() },
            }],
            label: None,
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
        assert_eq!(topo.bands.len(), 2);
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
        assert_eq!(topo.collections[0].collection.as_str(), "public.roads");
    }

    #[test]
    fn build_replication_topology_rejects_empty_layer_set() {
        let cfg = cfg_with_layers(vec![]);
        assert!(build_replication_topology(&cfg).is_err());
    }

    #[test]
    fn build_replication_topology_rejects_missing_band_size() {
        let mut cfg = cfg_with_layers(vec![layer("a", vec![("public.roads", "geom")])]);
        cfg.cells.size_per_band.remove("lo");
        let err = build_replication_topology(&cfg).unwrap_err().to_string();
        assert!(err.contains("missing entry for band 'lo'"), "{err}");
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
}

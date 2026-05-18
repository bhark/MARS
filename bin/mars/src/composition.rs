//! Composition helpers: turn parsed configuration into adapter wiring.
//!
//! Lives in the bin crate because adapter-shaped types (here:
//! `mars_source_postgres::ReplicationTopology`) are concrete; library crates
//! must not name them directly per the hexagonal-architecture rules.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use mars_config::{Config, config_dir};
use mars_runtime::RasterSourceRegistry;
use mars_source::RasterSource;
use mars_source_postgres::{CollectionTopology, ReplicationTopology, SourceCollectionId};
use mars_source_xyz::XyzRasterSource;

/// Parse the YAML at `path` and run cross-cutting validation. Used by every
/// entry point that needs a fully validated `Config` (service modes and the
/// `setup` / `teardown` tooling).
pub(crate) fn load_and_validate(path: &Path) -> Result<Config> {
    let mut cfg = mars_config::load(path).with_context(|| format!("load {}", path.display()))?;
    mars_config::validate(&mut cfg, &config_dir(path)).context("validate config")?;
    Ok(cfg)
}

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

/// Validate the change-feed configuration block on the unique postgis source
/// for compiler mode. Runtime mode never reads it, so this is only called
/// from the compiler boot path.
pub(crate) fn validate_change_feed_config(cfg: &Config) -> Result<()> {
    let pg = mars_bin_shared::unique_postgis_source(cfg)?
        .postgis()
        .ok_or_else(|| anyhow!("internal: unique_postgis_source returned non-postgis source"))?;
    let feed = pg
        .change_feed
        .as_ref()
        .ok_or_else(|| anyhow!("sources[].change_feed is required for compiler / all-in-one mode"))?;
    match feed.kind.as_str() {
        "pgoutput" => {
            let publication = feed.publication.as_deref().unwrap_or("");
            let slot = feed.slot.as_deref().unwrap_or("");
            if publication.is_empty() {
                return Err(anyhow!(
                    "sources[].change_feed.publication is required for type=pgoutput"
                ));
            }
            if slot.is_empty() {
                return Err(anyhow!("sources[].change_feed.slot is required for type=pgoutput"));
            }
            Ok(())
        }
        other => Err(anyhow!(
            "sources[].change_feed.type='{other}' unsupported; only 'pgoutput' is wired"
        )),
    }
}

/// Build the per-collection raster source registry the runtime hands to its
/// raster render path. One shared [`reqwest::Client`] backs every XYZ
/// collection (connection pooling per upstream host is reqwest's job). The
/// client honours `render.xyz_client` for timeouts and User-Agent. The
/// returned registry is keyed by `RasterLayerEntry.collection`; an empty
/// config (no raster layers) yields an empty registry and zero adapter
/// allocations.
pub(crate) fn build_raster_sources(cfg: &Config) -> Result<RasterSourceRegistry> {
    let mut out = RasterSourceRegistry::new();
    let mut xyz_source: Option<Arc<dyn RasterSource>> = None;
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
        let source = if let Some(existing) = xyz_source.as_ref() {
            existing.clone()
        } else {
            let built =
                Arc::new(XyzRasterSource::new(build_xyz_client(&cfg.render.xyz_client)?)) as Arc<dyn RasterSource>;
            xyz_source = Some(built.clone());
            built
        };
        out.insert(collection, source);
    }
    Ok(out)
}

fn build_xyz_client(cfg: &mars_config::XyzClient) -> Result<reqwest::Client> {
    let timeout = cfg.timeout().map_err(|e| anyhow!("render.xyz_client.timeout: {e}"))?;
    let connect_timeout = cfg
        .connect_timeout()
        .map_err(|e| anyhow!("render.xyz_client.connect_timeout: {e}"))?;
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(connect_timeout)
        .user_agent(&cfg.user_agent)
        .build()
        .map_err(|e| anyhow!("building XYZ HTTP client: {e}"))
}

#[cfg(test)]
mod tests;

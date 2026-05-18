//! Composition helpers shared by the `mars` and `mars-compile` bin crates.
//!
//! Both bins are composition roots that wire concrete adapters from parsed
//! configuration. The wiring is identical; this crate keeps it in one place
//! so the two bins cannot drift.
//!
//! Lives under `bin/` because it names concrete adapter types
//! (`PgSource`, `FsStore`, `S3Store`, ...) which the hexagonal-architecture
//! rules forbid in `crates/`.

#![forbid(unsafe_code)]

mod fonts;
mod stylesheet;
pub use fonts::load_fonts;
pub use stylesheet::build_stylesheet;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use mars_compiler::SourceRegistry;
use mars_config::{Config, Source as CfgSource, SourceBackend};
use mars_source::Source;
use mars_source_postgres::{PgConfig, PgSource, ReplicationTopology};
use mars_source_vectorfile::VectorFileSource;
use mars_store::{ManifestStore, ObjectStore};
use mars_store_fs::{FsPublisher, FsStore};
use mars_store_s3::{S3Config, S3Publisher, S3Store};

/// Locate the unique postgis source. Compiler / all-in-one mode requires
/// exactly one; runtime mode allows any number. Returns the source entry
/// so callers can name the id and read the `PostgisBackend` config.
pub fn unique_postgis_source(cfg: &Config) -> Result<&CfgSource> {
    let mut pg_sources = cfg.sources.iter().filter(|s| s.postgis().is_some());
    let first = pg_sources
        .next()
        .ok_or_else(|| anyhow!("at least one postgis source is required"))?;
    if pg_sources.next().is_some() {
        return Err(anyhow!(
            "exactly one postgis source is supported in this mode; multi-postgis compile is not implemented"
        ));
    }
    Ok(first)
}

/// Build a `PgSource` from the unique postgis backend in `cfg`.
///
/// `topology` is only relevant for compiler / all-in-one mode; pass `None`
/// for snapshot compiles. Errors if `cfg` does not have exactly one postgis
/// source.
pub async fn build_pg_source(cfg: &Config, topology: Option<ReplicationTopology>) -> Result<Arc<PgSource>> {
    let src_cfg = unique_postgis_source(cfg)?;
    let pg = src_cfg
        .postgis()
        .ok_or_else(|| anyhow!("source {} is not a postgis backend", src_cfg.id.as_str()))?;
    let binding_dsns = collect_binding_dsns(cfg, &src_cfg.id);
    let src = connect_pg(pg, topology, binding_dsns).await?;
    Ok(Arc::new(src))
}

/// Gather every unique per-binding DSN override declared against `source_id`
/// in `cfg`. The returned vector is sorted + deduplicated so the adapter can
/// build one pool per distinct DSN without re-deduping. Bindings without
/// override (the common case) contribute nothing.
fn collect_binding_dsns(cfg: &Config, source_id: &mars_config::SourceId) -> Vec<String> {
    let mut dsns: Vec<String> =
        cfg.layers
            .iter()
            .flat_map(|layer| layer.sources.iter())
            .filter(|b| &b.source == source_id)
            .filter_map(|b| match &b.kind {
                mars_config::BindingKind::PostgisTable { dsn, .. }
                | mars_config::BindingKind::PostgisSql { dsn, .. } => dsn.clone(),
                mars_config::BindingKind::Vectorfile { .. } => None,
            })
            .collect();
    dsns.sort();
    dsns.dedup();
    dsns
}

async fn connect_pg(
    pg: &mars_config::PostgisBackend,
    topology: Option<ReplicationTopology>,
    binding_dsns: Vec<String>,
) -> Result<PgSource> {
    let pool = &pg.pool;
    // change_feed config is meaningless without replication topology - skip it
    // entirely in snapshot mode so we don't ship empty publication/slot strings
    // that look like a configuration request.
    let feed = topology.is_some().then_some(pg.change_feed.as_ref()).flatten();
    let pg_cfg = PgConfig {
        dsn: pg.dsn.clone(),
        binding_dsns,
        publication: feed.and_then(|f| f.publication.clone()).unwrap_or_default(),
        slot: feed.and_then(|f| f.slot.clone()).unwrap_or_default(),
        max_pool_size: pool.max_size,
        recycle_timeout: pool.recycle_timeout_secs.map(Duration::from_secs),
        statement_timeout: pool.statement_timeout_ms.map(Duration::from_millis),
        fetch_concurrency: pool.fetch_concurrency,
        batch_send_timeout: pool.batch_send_timeout_secs.map(Duration::from_secs),
    };
    let src = PgSource::connect(pg_cfg).await.context("connect postgres")?;
    Ok(match topology {
        Some(t) => src.with_topology(t),
        None => src,
    })
}

/// Build the [`SourceRegistry`] the compiler uses to route bindings.
///
/// Iterates `cfg.sources`: each postgis entry connects a fresh `PgSource`
/// (`pg_main` reuses the caller-supplied instance, so the topology /
/// change-feed / leader-lock wiring stays consistent with the rest of the
/// compose root); each vectorfile entry constructs a `VectorFileSource`.
///
/// `pg_main` is the postgis source already constructed by `build_pg_source`
/// in compiler / all-in-one mode (so the topology applied there carries
/// through). Pass `None` in pure runtime or snapshot mode; the function
/// will connect any postgis sources fresh without topology.
pub async fn build_sources(cfg: &Config, pg_main: Option<Arc<PgSource>>) -> Result<SourceRegistry> {
    if cfg.sources.is_empty() {
        return Err(anyhow!("no sources configured; at least one entry is required"));
    }
    let mut registry = SourceRegistry::new();
    let mut pg_main = pg_main;
    for src_cfg in &cfg.sources {
        let source: Arc<dyn Source> = match &src_cfg.backend {
            SourceBackend::Postgis(pg) => match pg_main.take() {
                Some(existing) => existing,
                None => {
                    let binding_dsns = collect_binding_dsns(cfg, &src_cfg.id);
                    Arc::new(connect_pg(pg, None, binding_dsns).await?)
                }
            },
            SourceBackend::VectorFile(vf) => Arc::new(
                VectorFileSource::new(src_cfg.id.clone(), src_cfg.native_crs.clone(), vf.clone())
                    .await
                    .with_context(|| format!("init vectorfile source '{}'", src_cfg.id.as_str()))?,
            ),
        };
        registry.insert(src_cfg.id.clone(), source);
    }
    Ok(registry)
}

/// Build the artifact object store and the manifest publisher from the
/// `artifacts.store` block. Supports `fs` and `s3`.
///
/// Both halves come back together because they normally share a backend
/// configuration; constructing them independently would re-validate the
/// same fields and risk drift.
pub fn build_store_and_publisher(cfg: &Config) -> Result<(Arc<dyn ObjectStore>, Arc<dyn ManifestStore>)> {
    match cfg.artifacts.store.kind.as_str() {
        "fs" => {
            let p = cfg
                .artifacts
                .store
                .path
                .as_deref()
                .ok_or_else(|| anyhow!("artifacts.store.path required for type=fs"))?;
            let store: Arc<dyn ObjectStore> = Arc::new(FsStore::new(p).context("open fs store")?);
            let publisher: Arc<dyn ManifestStore> = Arc::new(FsPublisher::new(p).context("open fs manifest store")?);
            Ok((store, publisher))
        }
        "s3" => {
            let bucket = cfg
                .artifacts
                .store
                .bucket
                .clone()
                .ok_or_else(|| anyhow!("artifacts.store.bucket required for type=s3"))?;
            let region = std::env::var("AWS_REGION")
                .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
                .map_err(|_| anyhow!("AWS_REGION env required for type=s3"))?;
            let endpoint_is_plaintext = cfg
                .artifacts
                .store
                .endpoint
                .as_deref()
                .is_some_and(|e| e.starts_with("http://"));
            if endpoint_is_plaintext && !cfg.artifacts.store.allow_http {
                return Err(anyhow!(
                    "artifacts.store.endpoint uses http://; set artifacts.store.allow_http=true to permit plaintext"
                ));
            }
            // explicit env-cred passthrough: object_store's default chain still
            // probes imds (169.254.169.254) and ignores AWS_EC2_METADATA_DISABLED,
            // which stalls hot starts for ~14s/request in environments without
            // ec2 metadata. binding both env vars onto the builder short-circuits
            // the chain entirely; absence preserves the chain for IRSA / instance
            // profile / shared-creds workflows.
            let (access_key_id, secret_access_key) = match (
                std::env::var("AWS_ACCESS_KEY_ID").ok(),
                std::env::var("AWS_SECRET_ACCESS_KEY").ok(),
            ) {
                (Some(a), Some(s)) if !a.is_empty() && !s.is_empty() => (Some(a), Some(s)),
                _ => (None, None),
            };
            let s3 = S3Config {
                endpoint: cfg.artifacts.store.endpoint.clone(),
                region,
                bucket,
                prefix: cfg.artifacts.store.prefix.clone().unwrap_or_default(),
                access_key_id,
                secret_access_key,
                allow_http: endpoint_is_plaintext,
                allow_non_atomic_publish: cfg.artifacts.store.allow_non_atomic_publish,
                conditional_put: cfg.artifacts.store.conditional_put.clone(),
            };
            let store_inner = S3Store::from_config(&s3).context("open s3 store")?;
            let publisher: Arc<dyn ManifestStore> = Arc::new(
                S3Publisher::from_store(&store_inner).with_allow_non_atomic_publish(s3.allow_non_atomic_publish),
            );
            let store: Arc<dyn ObjectStore> = Arc::new(store_inner);
            Ok((store, publisher))
        }
        other => Err(anyhow!("artifacts.store.type='{other}' unsupported; use 'fs' or 's3'")),
    }
}

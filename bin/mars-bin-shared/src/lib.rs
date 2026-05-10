//! Composition helpers shared by the `mars` and `mars-compile` bin crates.
//!
//! Both bins are composition roots that wire concrete adapters from parsed
//! configuration. The wiring is identical; this crate keeps it in one place
//! so the two bins cannot drift (e.g. `mars-compile` previously rejected an
//! `s3`-typed store).
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
use mars_config::Config;
use mars_source_postgres::{PgConfig, PgSource, ReplicationTopology};
use mars_store::{ManifestStore, ObjectStore};
use mars_store_fs::{FsPublisher, FsStore};
use mars_store_s3::{S3Config, S3Publisher, S3Store};

/// Build a `PgSource` from the connection / pool block in `cfg`.
///
/// `topology` is only relevant for compiler / all-in-one mode; pass `None`
/// for snapshot compiles.
pub async fn build_pg_source(cfg: &Config, topology: Option<ReplicationTopology>) -> Result<Arc<PgSource>> {
    if cfg.source.kind != "postgis" {
        return Err(anyhow!(
            "source.type='{}' is not supported; only 'postgis' is implemented",
            cfg.source.kind
        ));
    }
    let pool = &cfg.source.pool;
    // change_feed config is meaningless without replication topology - skip it
    // entirely in snapshot mode so we don't ship empty publication/slot strings
    // that look like a configuration request.
    let feed = topology.is_some().then_some(cfg.source.change_feed.as_ref()).flatten();
    let pg_cfg = PgConfig {
        dsn: cfg.source.dsn.clone(),
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
        Some(t) => Arc::new(src.with_topology(t)),
        None => Arc::new(src),
    })
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
            let s3 = S3Config {
                endpoint: cfg.artifacts.store.endpoint.clone(),
                region,
                bucket,
                prefix: cfg.artifacts.store.prefix.clone().unwrap_or_default(),
                access_key_id: None,
                secret_access_key: None,
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

//! mars-compile: standalone snapshot compile CLI. Reuses `mars-compiler`
//! over a static source snapshot - useful for local dev, CI fixtures and
//! offline rebuilds. SPEC §18.2.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use mars_compiler::{Compiler, Deps};
use mars_config::{Config, config_dir};
use mars_source_postgres::{PgConfig, PgSource};
use mars_store::{ManifestStore, ObjectStore};
use mars_store_fs::{FsPublisher, FsStore};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Parser)]
#[command(
    name = "mars-compile",
    version,
    about = "Snapshot compile: build artifacts once and exit.",
    long_about = "Standalone snapshot compile. Builds artifacts once from the configured \
                  source and exits. The long-running compiler loop lives in the `mars` binary \
                  under `--mode compiler`."
)]
struct Cli {
    /// Path to the service configuration.
    #[arg(long, default_value = "/etc/mars/mars.yaml")]
    config: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async move {
        let cfg = mars_config::load(&cli.config).with_context(|| format!("load {}", cli.config.display()))?;
        let log_level = cfg.observability.log_level.clone();
        if let Err(e) = mars_observability::init_tracing(false, log_level.as_deref()) {
            eprintln!("warning: tracing init failed: {e}");
        }
        mars_config::validate(&cfg, &config_dir(&cli.config)).context("validate config")?;
        run_snapshot(cfg).await
    })
}

async fn run_snapshot(cfg: Config) -> Result<()> {
    let source = build_source(&cfg).await?;
    let store = build_store(&cfg)?;
    let manifest = build_publisher(&cfg)?;
    let metrics = mars_observability::Metrics::new().context("init metrics")?;

    let compiler = Compiler::new(
        Deps {
            source: source.clone(),
            change_feed: source.clone(),
            leader_lock: source,
            store,
            manifest,
            metrics,
        },
        cfg,
    );
    match compiler.run_snapshot_once(CancellationToken::new()).await {
        Ok(_) => Ok(()),
        Err(mars_compiler::CompilerError::NotLeader) => {
            eprintln!("compiler: another instance is leader; exiting cleanly");
            Ok(())
        }
        Err(e) => Err(anyhow!(e)),
    }
}

async fn build_source(cfg: &Config) -> Result<Arc<PgSource>> {
    if cfg.source.kind != "postgis" {
        return Err(anyhow!(
            "source.type='{}' is not supported in Phase 0; only 'postgis'",
            cfg.source.kind
        ));
    }
    let feed = cfg.source.change_feed.as_ref();
    let pool = &cfg.source.pool;
    let pg_cfg = PgConfig {
        dsn: cfg.source.dsn.clone(),
        publication: feed.and_then(|f| f.publication.clone()).unwrap_or_default(),
        slot: feed.and_then(|f| f.slot.clone()).unwrap_or_default(),
        max_pool_size: pool.max_size,
        recycle_timeout: pool.recycle_timeout_secs.map(std::time::Duration::from_secs),
        statement_timeout: pool.statement_timeout_ms.map(std::time::Duration::from_millis),
    };
    let src = PgSource::connect(pg_cfg).await.context("connect postgres")?;
    Ok(Arc::new(src))
}

fn build_store(cfg: &Config) -> Result<Arc<dyn ObjectStore>> {
    match cfg.artifacts.store.kind.as_str() {
        "fs" => {
            let path = cfg
                .artifacts
                .store
                .path
                .as_deref()
                .ok_or_else(|| anyhow!("artifacts.store.path required for type=fs"))?;
            Ok(Arc::new(FsStore::new(path).context("open fs store")?))
        }
        other => Err(anyhow!(
            "artifacts.store.type='{other}' not supported in Phase 0; use type=fs"
        )),
    }
}

fn build_publisher(cfg: &Config) -> Result<Arc<dyn ManifestStore>> {
    let path = cfg
        .artifacts
        .store
        .path
        .as_deref()
        .ok_or_else(|| anyhow!("artifacts.store.path required for manifest store"))?;
    Ok(Arc::new(FsPublisher::new(path).context("open fs manifest store")?))
}

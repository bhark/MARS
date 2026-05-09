//! mars-compile: standalone snapshot compile CLI. Reuses `mars-compiler`
//! over a static source snapshot - useful for local dev, CI fixtures and
//! offline rebuilds. SPEC §18.2.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use mars_bin_shared::{build_pg_source, build_store_and_publisher};
use mars_compiler::{Compiler, Deps};
use mars_config::{Config, config_dir};
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
        let mut cfg = mars_config::load(&cli.config).with_context(|| format!("load {}", cli.config.display()))?;
        let log_level = cfg.observability.log_level.clone();
        if let Err(e) = mars_observability::init_tracing(false, log_level.as_deref()) {
            eprintln!("warning: tracing init failed: {e}");
        }
        mars_config::validate(&mut cfg, &config_dir(&cli.config)).context("validate config")?;
        run_snapshot(cfg).await
    })
}

async fn run_snapshot(cfg: Config) -> Result<()> {
    // snapshot compile: no replication topology, no leader contention.
    let source = build_pg_source(&cfg, None).await?;
    let (store, manifest) = build_store_and_publisher(&cfg)?;
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

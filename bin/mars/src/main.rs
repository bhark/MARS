//! MARS service binary. Composition root.
//!
//! `mars --mode {runtime|compiler|all-in-one} --config /etc/mars/mars.yaml`
//! is the service operation path.
//!
//! `mars validate <path>` and `mars inspect <path>` are operational tooling
//! subcommands. Providing both `--mode` and a subcommand is a parse error.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Parser)]
#[command(
    name = "mars",
    version,
    about = "MARS - Map Artifact Rendering Service",
    long_about = None,
)]
struct Cli {
    /// Service operation mode. Required for service operation; mutually
    /// exclusive with subcommands.
    #[arg(long, value_enum)]
    mode: Option<Mode>,

    /// Path to the service configuration file.
    #[arg(long, default_value = "/etc/mars/mars.yaml")]
    config: PathBuf,

    /// Operational tooling. Mutually exclusive with `--mode`.
    #[command(subcommand)]
    tool: Option<Tool>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Mode {
    /// Serve WMS / WMTS / health / metrics. Stateless. Multiple replicas allowed.
    Runtime,
    /// Subscribe to the source change feed, build artifacts, publish manifests.
    /// Singleton per service.
    Compiler,
    /// Both compiler and runtime in one process. Dev / tiny deployments only.
    AllInOne,
}

#[derive(Debug, Subcommand)]
enum Tool {
    /// Validate a configuration file: parse YAML, check expressions, ping
    /// the source DB, dry-run the change-feed setup.
    Validate {
        /// Path to the configuration file.
        path: PathBuf,
    },
    /// Inspect a `.mars` artifact: footer, sections, hashes, bbox, schema.
    Inspect {
        /// Path to the artifact file.
        path: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    mars_observability::init_tracing(false).ok();

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> Result<()> {
    match (cli.mode, cli.tool) {
        (Some(_), Some(_)) => {
            anyhow::bail!("mars: --mode and a subcommand are mutually exclusive; provide exactly one")
        }
        (None, None) => {
            anyhow::bail!("mars: provide --mode <runtime|compiler|all-in-one> or one of: validate, inspect")
        }
        (Some(Mode::Runtime), None) => run_runtime(&cli.config).await,
        (Some(Mode::Compiler), None) => run_compiler(&cli.config).await,
        (Some(Mode::AllInOne), None) => run_all_in_one(&cli.config).await,
        (None, Some(Tool::Validate { path })) => tool_validate(&path).await,
        (None, Some(Tool::Inspect { path })) => tool_inspect(&path).await,
    }
}

async fn run_runtime(config_path: &Path) -> Result<()> {
    tracing::info!(?config_path, "starting runtime mode (Phase 0 stub)");
    let _cfg = mars_config::load(config_path);
    let renderer = Arc::new(mars_render::StubRenderer);
    let store = Arc::new(mars_store_fs::StubFs::default());
    let cache = Arc::new(mars_store_fs::StubFs::default());
    let runtime = Arc::new(mars_runtime::Runtime::new(mars_runtime::Deps {
        store,
        cache,
        renderer,
    }));
    let cfg = mars_http::ServerConfig {
        listen: "0.0.0.0:8080".parse()?,
        debug_endpoints: false,
    };
    // phase 0: serve returns NotImplemented; that's the verification goal.
    if let Err(e) = mars_http::serve(cfg, runtime).await {
        tracing::error!(%e, "http serve returned error (expected in Phase 0)");
        return Err(e.into());
    }
    Ok(())
}

async fn run_compiler(config_path: &Path) -> Result<()> {
    tracing::info!(?config_path, "starting compiler mode (Phase 0 stub)");
    let _cfg = mars_config::load(config_path);
    let pg = Arc::new(mars_source_postgres::StubPg::default());
    let store = Arc::new(mars_store_s3::StubS3::default());
    let manifest_pub: Arc<dyn mars_store::ManifestPublisher> =
        Arc::new(mars_store::stub::NotImplementedPublisher);
    let compiler = mars_compiler::Compiler::new(mars_compiler::Deps {
        source: pg.clone(),
        change_feed: pg,
        store,
        manifest: manifest_pub,
    });
    if let Err(e) = compiler.run(CancellationToken::new()).await {
        tracing::error!(%e, "compiler run returned error (expected in Phase 0)");
        return Err(e.into());
    }
    Ok(())
}

async fn run_all_in_one(config_path: &Path) -> Result<()> {
    tracing::info!(?config_path, "starting all-in-one mode (Phase 0 stub)");
    let (a, b) = tokio::join!(run_compiler(config_path), run_runtime(config_path));
    a.and(b)
}

async fn tool_validate(path: &Path) -> Result<()> {
    tracing::info!(?path, "validate (Phase 0 stub)");
    match mars_config::load(path) {
        Ok(_) => {
            println!("ok");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

async fn tool_inspect(path: &Path) -> Result<()> {
    tracing::info!(?path, "inspect (Phase 0 stub)");
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| anyhow::anyhow!("read {}: {}", path.display(), e))?;
    let _reader = mars_artifact::ArtifactReader::open(bytes::Bytes::from(bytes))?;
    println!("opened artifact ok");
    Ok(())
}

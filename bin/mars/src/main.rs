//! MARS service binary. Composition root.
//!
//! `mars --mode {runtime|compiler|all-in-one} --config /etc/mars/mars.yaml`
//! is the service operation path.
//!
//! `mars validate <path>` and `mars inspect <path>` are operational tooling
//! subcommands. Providing both `--mode` and a subcommand is a parse error.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand, ValueEnum};
use mars_compiler::{Compiler, Deps as CompilerDeps};
use mars_config::{ClassStyle, Config, StyleEntry, config_dir};
use mars_render::TinySkiaRenderer;
use mars_runtime::{Deps as RuntimeDeps, Runtime, RuntimeState, run_manifest_reload_loop};
use mars_source_postgres::{PgConfig, PgSource};
use mars_store::{LocalCache, ManifestReader, ObjectStore};
use mars_store_fs::{FsCache, FsPublisher, FsStore};
use mars_style::Stylesheet;
use mars_types::Manifest;
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
    if let Err(e) = mars_observability::init_tracing(false) {
        eprintln!("warning: tracing init failed: {e}");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> Result<()> {
    match (cli.mode, cli.tool) {
        (Some(_), Some(_)) => Err(anyhow!(
            "mars: --mode and a subcommand are mutually exclusive; provide exactly one"
        )),
        (None, None) => Err(anyhow!(
            "mars: provide --mode <runtime|compiler|all-in-one> or one of: validate, inspect"
        )),
        (Some(Mode::Runtime), None) => run_runtime(&cli.config).await,
        (Some(Mode::Compiler), None) => run_compiler_mode(&cli.config).await,
        (Some(Mode::AllInOne), None) => run_all_in_one(&cli.config).await,
        (None, Some(Tool::Validate { path })) => tool_validate(&path).await,
        (None, Some(Tool::Inspect { path })) => tool_inspect(&path).await,
    }
}

// ---------- runtime mode ----------

async fn run_runtime(config_path: &Path) -> Result<()> {
    let cfg = load_and_validate(config_path)?;
    let cfg = Arc::new(cfg);

    let store = build_store(&cfg)?;
    let cache = build_cache(&cfg)?;
    let publisher = build_publisher(&cfg)?;
    let stylesheet = build_stylesheet(&cfg);

    let listen = resolve_listen(&cfg)?;
    let wms_cfg = mars_wms::WmsConfig::from_config(&cfg);
    let runtime = Arc::new(Runtime::empty(RuntimeDeps {
        store,
        cache,
        renderer: Arc::new(TinySkiaRenderer),
    }));

    let manifest_opt = match publisher.current_manifest().await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "initial manifest unavailable");
            None
        }
    };

    match &manifest_opt {
        Some(manifest) => match RuntimeState::from_config_and_manifest(&cfg, stylesheet.clone(), manifest.clone()) {
            Ok(state) => runtime.swap_state(Arc::new(state)),
            Err(e) => tracing::warn!(error = %e, "initial manifest rejected"),
        },
        None => {
            tracing::warn!("no manifest published yet; readyz will return 503");
        }
    }

    let watcher: Arc<dyn mars_store::ManifestWatch> = publisher.clone();
    let _reload_task = tokio::spawn({
        let runtime = runtime.clone();
        let cfg = cfg.clone();
        async move {
            if let Err(e) = run_manifest_reload_loop(runtime, watcher, cfg, stylesheet).await {
                tracing::error!(error = %e, "manifest reload loop stopped");
            }
        }
    });

    let caps = match &manifest_opt {
        Some(manifest) => mars_wms::capabilities_xml(&cfg, manifest),
        None => mars_wms::capabilities_xml(&cfg, &empty_manifest(&cfg)),
    }
    .map_err(|e| anyhow!("capabilities: {e}"))?;

    mars_http::serve(
        mars_http::ServerConfig {
            listen,
            debug_endpoints: false,
        },
        runtime,
        caps,
        wms_cfg,
    )
    .await
    .map_err(Into::into)
}

// ---------- compiler mode ----------

async fn run_compiler_mode(config_path: &Path) -> Result<()> {
    let cfg = load_and_validate(config_path)?;
    run_compiler(cfg).await
}

async fn run_compiler(cfg: Config) -> Result<()> {
    let source = build_source(&cfg).await?;
    let store = build_store(&cfg)?;
    let publisher = build_publisher(&cfg)?;

    let compiler = Compiler::new(
        CompilerDeps {
            source: source.clone(),
            change_feed: source,
            store,
            manifest: publisher,
        },
        cfg,
    );
    compiler.run(CancellationToken::new()).await.map_err(|e| anyhow!(e))
}

async fn run_all_in_one(config_path: &Path) -> Result<()> {
    let cfg = load_and_validate(config_path)?;
    run_compiler(cfg).await?;
    run_runtime(config_path).await
}

// ---------- tooling ----------

async fn tool_validate(path: &Path) -> Result<()> {
    let cfg = mars_config::load(path).with_context(|| format!("load {}", path.display()))?;
    mars_config::validate(&cfg, &config_dir(path)).context("validate")?;
    println!("ok");
    Ok(())
}

async fn tool_inspect(path: &Path) -> Result<()> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| anyhow!("read {}: {}", path.display(), e))?;
    let reader = mars_artifact::ArtifactReader::open(bytes::Bytes::from(bytes))?;
    let bbox = reader.bbox();
    println!("kind: {:?}", reader.kind());
    println!("bbox: [{}, {}, {}, {}]", bbox.min_x, bbox.min_y, bbox.max_x, bbox.max_y);
    println!("feature_count: {}", reader.feature_count());
    if let Some(sr) = reader.source_ref() {
        println!(
            "source_ref: collection={} band={} cell=({},{})",
            sr.collection, sr.band, sr.cell_x, sr.cell_y
        );
    }
    println!("sections:");
    for kind in [
        mars_artifact::SectionKind::GeometryIndex,
        mars_artifact::SectionKind::GeometryPayload,
        mars_artifact::SectionKind::Attributes,
        mars_artifact::SectionKind::LabelCandidates,
        mars_artifact::SectionKind::ClassAssignment,
        mars_artifact::SectionKind::StyleRefs,
    ] {
        match reader.section(kind) {
            Ok(b) => println!("  - {kind:?}: {} bytes", b.len()),
            Err(mars_artifact::ArtifactError::SectionMissing(_)) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

// ---------- composition helpers ----------

fn load_and_validate(path: &Path) -> Result<Config> {
    let cfg = mars_config::load(path).with_context(|| format!("load {}", path.display()))?;
    mars_config::validate(&cfg, &config_dir(path)).context("validate config")?;
    Ok(cfg)
}

async fn build_source(cfg: &Config) -> Result<Arc<PgSource>> {
    if cfg.source.kind != "postgis" {
        return Err(anyhow!(
            "source.type='{}' not supported in Phase 0; only 'postgis'",
            cfg.source.kind
        ));
    }
    let feed = cfg.source.change_feed.as_ref();
    let pg_cfg = PgConfig {
        dsn: cfg.source.dsn.clone(),
        publication: feed.and_then(|f| f.publication.clone()).unwrap_or_default(),
        slot: feed.and_then(|f| f.slot.clone()).unwrap_or_default(),
    };
    Ok(Arc::new(PgSource::connect(pg_cfg).await.context("connect postgres")?))
}

fn build_store(cfg: &Config) -> Result<Arc<dyn ObjectStore>> {
    match cfg.artifacts.store.kind.as_str() {
        "fs" => {
            let p = cfg
                .artifacts
                .store
                .path
                .as_deref()
                .ok_or_else(|| anyhow!("artifacts.store.path required for type=fs"))?;
            Ok(Arc::new(FsStore::new(p).context("open fs store")?))
        }
        other => Err(anyhow!(
            "artifacts.store.type='{other}' not supported in Phase 0; use type=fs"
        )),
    }
}

fn build_cache(cfg: &Config) -> Result<Arc<dyn LocalCache>> {
    let max = cfg
        .artifacts
        .cache
        .max_size_bytes()
        .map_err(|e| anyhow!("parse cache max_size: {e}"))?;
    Ok(Arc::new(
        FsCache::new(&cfg.artifacts.cache.path, max).context("open fs cache")?,
    ))
}

fn build_publisher(cfg: &Config) -> Result<Arc<FsPublisher>> {
    let p = cfg
        .artifacts
        .store
        .path
        .as_deref()
        .ok_or_else(|| anyhow!("artifacts.store.path required for manifest publisher"))?;
    Ok(Arc::new(FsPublisher::new(p).context("open fs publisher")?))
}

fn build_stylesheet(cfg: &Config) -> Stylesheet {
    let mut ss = Stylesheet::default();
    for (name, entry) in &cfg.styles {
        match entry {
            StyleEntry::Geometry(s) => {
                ss.geometry.insert(name.clone(), s.clone());
            }
            StyleEntry::Label(l) => {
                ss.labels.insert(name.clone(), l.style.clone());
            }
        }
    }
    // also collect inline class styles under `<layer>::<class>` so runtime can
    // resolve them via the same map; refs are already covered above.
    for layer in &cfg.layers {
        for class in &layer.classes {
            if let ClassStyle::Inline(s) = &class.style {
                let key = format!("{}::{}", layer.name, class.name);
                ss.geometry.insert(key, s.clone());
            }
        }
    }
    ss
}

fn empty_manifest(cfg: &Config) -> Manifest {
    Manifest {
        version: 0,
        service: cfg.service.name.clone(),
        source_artifacts: vec![],
        layer_artifacts: vec![],
        style_artifact: None,
    }
}

fn resolve_listen(cfg: &Config) -> Result<SocketAddr> {
    let raw = cfg
        .interfaces
        .wms
        .as_ref()
        .and_then(|w| w.listen.clone())
        .or_else(|| std::env::var("MARS_HTTP_LISTEN").ok())
        .unwrap_or_else(|| "127.0.0.1:1337".to_owned());
    SocketAddr::from_str(&raw).with_context(|| format!("parse listen addr {raw:?}"))
}

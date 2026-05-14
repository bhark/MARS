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
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand, ValueEnum};
use futures_util::StreamExt;
use mars_bin_shared::{build_pg_source, build_store_and_publisher, build_stylesheet, load_fonts};
use mars_compiler::{Compiler, Deps as CompilerDeps};
use mars_config::{Config, PngCompression as ConfigPngCompression, config_dir};
use mars_render::{PngCompression as RenderPngCompression, TinySkiaEncoder, TinySkiaRenderer};
use mars_runtime::{Deps as RuntimeDeps, Runtime, RuntimeState, run_manifest_reload_loop};
use mars_store::{LocalCache, ManifestStore};
use mars_store_fs::FsCache;
use mars_types::Manifest;
use tokio_util::sync::CancellationToken;

mod composition;

#[derive(Debug, Parser)]
#[command(
    name = "mars",
    version,
    about = "MARS - Map Artifact Rendering Service",
    long_about = None,
    // top-level args (--mode, --config) are mutually exclusive with the
    // tooling subcommands. clap enforces this at parse time so renames or
    // new subcommands can't drift away from the constraint.
    args_conflicts_with_subcommands = true,
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
    /// Validate a configuration file: parse YAML and run cross-cutting checks.
    Validate {
        /// Path to the configuration file.
        path: PathBuf,
    },
    /// Inspect a `.mars` artifact: footer, sections, hashes, bbox, schema.
    Inspect {
        /// Path to the artifact file.
        path: PathBuf,
    },
    /// Perform an HTTP health check against a URL.
    /// Exits 0 on 2xx, 1 otherwise. Used by container health probes.
    Healthcheck {
        /// URL to GET.
        #[arg(long)]
        url: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // service modes need a validated Config; tool subcommands don't. load
    // once here so observability prefs and the chosen mode share one parse.
    let cfg = if cli.mode.is_some() {
        Some(Arc::new(load_and_validate(&cli.config)?))
    } else {
        None
    };

    let (json, log_level) = cfg.as_ref().map_or((false, None), |c| {
        (
            matches!(c.observability.log_format.as_deref(), Some("json")),
            c.observability.log_level.clone(),
        )
    });
    if let Err(e) = mars_observability::init_tracing(json, log_level.as_deref()) {
        eprintln!("warning: tracing init failed: {e}");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async_main(cli, cfg))
}

async fn async_main(cli: Cli, cfg: Option<Arc<Config>>) -> Result<()> {
    // clap's `conflicts_with` on `mode` rules out the (Some, Some) case at
    // parse time; only one branch can populate.
    match (cli.mode, cli.tool) {
        (None, None) => Err(anyhow!(
            "mars: provide --mode <runtime|compiler|all-in-one> or one of: validate, inspect"
        )),
        (Some(Mode::Runtime), None) => {
            let cfg = cfg.ok_or_else(|| anyhow!("internal: service mode without loaded config"))?;
            let shutdown = install_signal_handler();
            run_runtime(cfg, shutdown).await
        }
        (Some(Mode::Compiler), None) => {
            let cfg = cfg.ok_or_else(|| anyhow!("internal: service mode without loaded config"))?;
            let shutdown = install_signal_handler();
            run_compiler(cfg, shutdown).await
        }
        (Some(Mode::AllInOne), None) => {
            let cfg = cfg.ok_or_else(|| anyhow!("internal: service mode without loaded config"))?;
            let shutdown = install_signal_handler();
            run_all_in_one(cfg, shutdown).await
        }
        (None, Some(Tool::Validate { path })) => tool_validate(&path).await,
        (None, Some(Tool::Inspect { path })) => tool_inspect(&path).await,
        (None, Some(Tool::Healthcheck { url })) => tool_healthcheck(&url),
        (Some(_), Some(_)) => unreachable!("clap conflicts_with rules this out at parse time"),
    }
}

/// Spawn a SIGINT/SIGTERM watcher. The first signal cancels the returned
/// token (graceful shutdown). A second signal escalates to `exit(130)` so
/// operators can break out of a stuck drain.
fn install_signal_handler() -> CancellationToken {
    let token = CancellationToken::new();
    let watcher = token.clone();
    tokio::spawn(async move {
        if let Err(e) = wait_for_termination().await {
            tracing::warn!(error = %e, "signal handler unavailable; signal-based shutdown disabled");
            return;
        }
        tracing::info!("signal received; initiating graceful shutdown");
        watcher.cancel();
        // second signal escalates: force exit so a stuck task can't trap the
        // operator. exit code 130 = killed by SIGINT.
        if wait_for_termination().await.is_ok() {
            tracing::warn!("second signal received; forcing exit");
            std::process::exit(130);
        }
    });
    token
}

/// Resolve when either SIGINT (ctrl_c) or SIGTERM is received. Production
/// orchestrators (k8s, systemd) send SIGTERM at pod stop; without this the
/// graceful drain never runs and the kernel kills the process at the grace
/// deadline.
async fn wait_for_termination() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! {
            res = tokio::signal::ctrl_c() => res,
            _ = term.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

// ---------- runtime mode ----------

async fn run_runtime(cfg: Arc<Config>, shutdown: CancellationToken) -> Result<()> {
    let (store, publisher) = build_store_and_publisher(&cfg)?;
    let cache = build_cache(&cfg)?;
    let stylesheet = build_stylesheet(&cfg);
    let fonts = load_fonts(&cfg)?;

    let listen = resolve_listen(&cfg)?;
    let wms_cfg = mars_wms::WmsConfig::from_config(&cfg);
    let wmts_cfg = mars_wmts::WmtsConfig::from_config(&cfg);
    let metrics = mars_observability::Metrics::new().context("init metrics")?;
    let pixel_budget = cfg
        .render
        .pixel_budget_permits()
        .context("resolve render.pixel_budget")?;
    let images = Arc::new(mars_runtime::images::MutableImageRegistry::new());
    let runtime = Arc::new(Runtime::with_pixel_budget(
        RuntimeDeps {
            store,
            cache,
            renderer: Arc::new(TinySkiaRenderer::with_images(fonts.clone(), images.clone())),
            encoder: Arc::new(TinySkiaEncoder::new(
                cfg.render.jpeg_quality,
                map_png_compression(cfg.render.png_compression),
            )),
            metrics: metrics.clone(),
            fonts,
            images,
            raster_sources: std::collections::HashMap::new(),
        },
        pixel_budget,
        None,
    ));

    let manifest_opt = match publisher.current().await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "initial manifest unavailable");
            None
        }
    };

    match &manifest_opt {
        Some(manifest) => {
            match mars_runtime::images::load_from_manifest(
                manifest.image_artifact.as_ref(),
                &runtime.deps().cache,
                &runtime.deps().store,
            )
            .await
            {
                Ok(map) => runtime.deps().images.set(map),
                Err(e) => tracing::warn!(error = %e, "initial image_artifact load failed"),
            }
            match RuntimeState::from_config_and_manifest(&cfg, stylesheet.clone(), manifest.clone()) {
                Ok(state) => runtime.swap_state(Arc::new(state)),
                Err(e) => tracing::warn!(error = %e, "initial manifest rejected"),
            }
        }
        None => {
            tracing::warn!("no manifest published yet; readyz will return 503");
        }
    }

    let manifests: Arc<dyn ManifestStore> = publisher.clone();
    let reload_task = tokio::spawn({
        let runtime = runtime.clone();
        let cfg = cfg.clone();
        let stylesheet = stylesheet.clone();
        let manifests = manifests.clone();
        let shutdown = shutdown.clone();
        async move {
            if let Err(e) = run_manifest_reload_loop(runtime, manifests, cfg, stylesheet, shutdown).await {
                tracing::error!(error = %e, "manifest reload loop stopped");
            }
        }
    });

    let initial_manifest_for_caps = manifest_opt.clone().unwrap_or_else(|| empty_manifest(&cfg));
    let initial_wms_caps =
        mars_wms::capabilities_xml(&cfg, &initial_manifest_for_caps).map_err(|e| anyhow!("wms capabilities: {e}"))?;
    let initial_wmts_caps =
        mars_wmts::capabilities_xml(&cfg, &initial_manifest_for_caps).map_err(|e| anyhow!("wmts capabilities: {e}"))?;
    let caps_bundle = mars_http::CapabilitiesBundle {
        wms: mars_http::capabilities_handle(initial_wms_caps),
        wmts: mars_http::capabilities_handle(initial_wmts_caps),
    };

    let caps_task = tokio::spawn(rebuild_capabilities_loop(
        manifests.clone(),
        cfg.clone(),
        caps_bundle.clone(),
        metrics.clone(),
        shutdown.clone(),
    ));

    let serve_result = mars_http::serve(
        mars_http::ServerConfig { listen },
        runtime,
        caps_bundle,
        wms_cfg,
        wmts_cfg,
        metrics,
        shutdown.clone(),
    )
    .await;

    // signal background loops to drain. capabilities/manifest watch streams
    // close when the underlying store is dropped, but cancelling now lets us
    // tear down promptly even when the watch is mid-poll.
    shutdown.cancel();
    let drain = Duration::from_secs(30);
    if tokio::time::timeout(drain, async {
        let _ = tokio::join!(reload_task, caps_task);
    })
    .await
    .is_err()
    {
        tracing::warn!("background tasks did not drain within {}s", drain.as_secs());
    }

    serve_result.map_err(Into::into)
}

/// Subscribe to the manifest watch stream and atomically swap the cached
/// capabilities body whenever the manifest changes. Errors on the watch are
/// logged; the task keeps running so transient adapter failures do not freeze
/// the capabilities document.
async fn rebuild_capabilities_loop(
    manifests: Arc<dyn ManifestStore>,
    cfg: Arc<Config>,
    handles: mars_http::CapabilitiesBundle,
    metrics: mars_observability::Metrics,
    shutdown: CancellationToken,
) {
    let mut stream = match manifests.watch().await {
        Ok(s) => s,
        Err(e) => {
            metrics.inc_capabilities_rebuild_failures();
            tracing::error!(error = %e, "capabilities: manifest watch unavailable");
            return;
        }
    };
    loop {
        let next = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            n = stream.next() => match n {
                Some(n) => n,
                None => return,
            },
        };
        let manifest = match next {
            Ok(m) => m,
            Err(e) => {
                metrics.inc_capabilities_rebuild_failures();
                tracing::warn!(error = %e, "capabilities: ignoring invalid snapshot");
                continue;
            }
        };
        match mars_wms::capabilities_xml(&cfg, &manifest) {
            Ok(body) => handles.wms.store(Arc::new(mars_http::CapabilitiesDoc::new(body))),
            Err(e) => {
                metrics.inc_capabilities_rebuild_failures();
                tracing::error!(error = %e, "capabilities: wms rebuild failed");
            }
        }
        match mars_wmts::capabilities_xml(&cfg, &manifest) {
            Ok(body) => handles.wmts.store(Arc::new(mars_http::CapabilitiesDoc::new(body))),
            Err(e) => {
                metrics.inc_capabilities_rebuild_failures();
                tracing::error!(error = %e, "capabilities: wmts rebuild failed");
            }
        }
    }
}

// ---------- compiler mode ----------

async fn run_compiler(cfg: Arc<Config>, shutdown: CancellationToken) -> Result<()> {
    composition::validate_change_feed_config(&cfg)?;
    let topology = composition::build_replication_topology(&cfg)?;
    let source = build_pg_source(&cfg, Some(topology)).await?;
    let (store, publisher) = build_store_and_publisher(&cfg)?;
    let metrics = mars_observability::Metrics::new().context("init metrics")?;

    // Compiler::new takes Config by value; clone out of the Arc once at handoff.
    let compiler = Compiler::new(
        CompilerDeps {
            source: source.clone(),
            change_feed: source.clone(),
            leader_lock: source,
            store,
            manifest: publisher,
            metrics,
        },
        (*cfg).clone(),
    );
    match compiler.run(shutdown).await {
        Ok(()) => Ok(()),
        Err(mars_compiler::CompilerError::NotLeader) => {
            tracing::info!("compiler: another instance is leader; exiting cleanly");
            Ok(())
        }
        Err(e) => Err(anyhow!(e)),
    }
}

async fn run_all_in_one(cfg: Arc<Config>, shutdown: CancellationToken) -> Result<()> {
    // spawn both halves so we can observe the first to finish and cancel the
    // shared shutdown *before* awaiting the survivor's drain. try_join! would
    // drop the survivor's future mid-await on a sibling failure, so its HTTP
    // graceful drain never runs. we want the survivor to see the cancellation
    // and shut down cleanly.
    let mut compiler_handle = tokio::spawn(run_compiler(cfg.clone(), shutdown.clone()));
    let mut runtime_handle = tokio::spawn(run_runtime(cfg, shutdown.clone()));

    let first = tokio::select! {
        res = &mut compiler_handle => ("compiler", res),
        res = &mut runtime_handle => ("runtime", res),
    };
    shutdown.cancel();

    let (first_name, first_res) = first;
    let (compiler_res, runtime_res) = if first_name == "compiler" {
        (first_res, runtime_handle.await)
    } else {
        (compiler_handle.await, first_res)
    };

    flatten_join(compiler_res, "compiler")?;
    flatten_join(runtime_res, "runtime")?;
    Ok(())
}

fn flatten_join(res: Result<Result<()>, tokio::task::JoinError>, what: &str) -> Result<()> {
    match res {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.context(format!("{what} task"))),
        Err(e) => Err(anyhow!(e).context(format!("{what} task panicked"))),
    }
}

// ---------- tooling ----------

fn tool_healthcheck(url: &str) -> Result<()> {
    let resp = reqwest::blocking::get(url).with_context(|| format!("healthcheck: request to {url}"))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(anyhow!("healthcheck: {url} returned {status}"))
    }
}

async fn tool_validate(path: &Path) -> Result<()> {
    let mut cfg = mars_config::load(path).with_context(|| format!("load {}", path.display()))?;
    mars_config::validate(&mut cfg, &config_dir(path)).context("validate")?;
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
        mars_artifact::SectionKind::SpatialIndex,
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
    // β.4: surface per-(layer, page) unmatched-slot diagnostic. when both
    // geometry and class-assignment are present, the difference is the
    // unmatched-slot count; β.2 should keep this at zero for single-layer-
    // per-binding pages. when only one is present (page-only or sidecar-
    // only artifact), report what's available so operators can cross-
    // reference manually.
    let geom_slots = match reader.section(mars_artifact::SectionKind::SpatialIndex) {
        Ok(b) => Some(mars_artifact::SpatialIndex::open(b)?.len() as usize),
        Err(mars_artifact::ArtifactError::SectionMissing(_)) => None,
        Err(e) => return Err(e.into()),
    };
    let class_slots = match reader.section(mars_artifact::SectionKind::ClassAssignment) {
        Ok(b) => Some(mars_artifact::decode_class_assignment(&b)?.len()),
        Err(mars_artifact::ArtifactError::SectionMissing(_)) => None,
        Err(e) => return Err(e.into()),
    };
    let label_slots = match reader.section(mars_artifact::SectionKind::LabelCandidates) {
        Ok(b) => Some(mars_artifact::decode_label_candidates(&b)?.len()),
        Err(mars_artifact::ArtifactError::SectionMissing(_)) => None,
        Err(e) => return Err(e.into()),
    };
    if let Some(g) = geom_slots {
        println!("geometry slots: {g}");
    }
    if let Some(c) = class_slots {
        println!("class assignments: {c}");
    }
    if let Some(l) = label_slots {
        println!("label candidates: {l}");
    }
    if let (Some(g), Some(c)) = (geom_slots, class_slots) {
        let unmatched = g.saturating_sub(c);
        println!("unmatched slots: {unmatched} (geom - class)");
    }
    Ok(())
}

// ---------- composition helpers ----------

fn load_and_validate(path: &Path) -> Result<Config> {
    let mut cfg = mars_config::load(path).with_context(|| format!("load {}", path.display()))?;
    mars_config::validate(&mut cfg, &config_dir(path)).context("validate config")?;
    Ok(cfg)
}

fn build_cache(cfg: &Config) -> Result<Arc<dyn LocalCache>> {
    let max = cfg
        .artifacts
        .cache
        .max_size_bytes()
        .map_err(|e| anyhow!("parse cache max_size: {e}"))?;
    Ok(Arc::new(
        FsCache::with_trust_path_hash(&cfg.artifacts.cache.path, max, cfg.artifacts.cache.trust_path_hash)
            .context("open fs cache")?,
    ))
}

fn empty_manifest(cfg: &Config) -> Manifest {
    Manifest::empty(0, cfg.service.name.clone())
}

const DEFAULT_LISTEN_ADDR: &str = "0.0.0.0:8080";

fn map_png_compression(c: ConfigPngCompression) -> RenderPngCompression {
    match c {
        ConfigPngCompression::None => RenderPngCompression::None,
        ConfigPngCompression::Fastest => RenderPngCompression::Fastest,
        ConfigPngCompression::Fast => RenderPngCompression::Fast,
        ConfigPngCompression::Balanced => RenderPngCompression::Balanced,
        ConfigPngCompression::High => RenderPngCompression::High,
    }
}

fn resolve_listen(cfg: &Config) -> Result<SocketAddr> {
    let raw = cfg
        .interfaces
        .wms
        .as_ref()
        .and_then(|w| w.listen.clone())
        .or_else(|| std::env::var("MARS_HTTP_LISTEN").ok())
        .unwrap_or_else(|| DEFAULT_LISTEN_ADDR.to_owned());
    SocketAddr::from_str(&raw).with_context(|| format!("parse listen addr {raw:?}"))
}

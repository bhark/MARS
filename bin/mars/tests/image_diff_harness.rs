//! image-diff harness: renders the parcels-mini fixture against a real
//! postgis container and compares the PNG output against a checked-in golden.
//! gated on the `e2e` cargo feature, like `e2e_render.rs`.
//!
//! regenerate the golden by setting `MARS_GOLDEN_REGENERATE=1`; see the
//! fixture README.

#![cfg(feature = "e2e")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use mars_compiler::{Compiler, Deps as CompilerDeps};
use mars_config::{ClassStyle, Config, config_dir};
use mars_render::{TinySkiaEncoder, TinySkiaRenderer};
use mars_runtime::{Deps as RuntimeDeps, RenderPlan, Runtime, RuntimeState};
use mars_source_postgres::{PgConfig, PgSource};
use mars_store::ManifestStore;
use mars_store_fs::{FsCache, FsPublisher, FsStore};
use mars_style::Stylesheet;
use mars_types::{Bbox, CrsCode, ImageFormat, LayerId};
use rand::distributions::{Alphanumeric, DistString};
use tempfile::TempDir;
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use tokio_postgres::NoTls;
use tokio_util::sync::CancellationToken;

mod common;
use common::{assert_within_tolerance, diff_pngs};

const RENDER_W: u32 = 512;
const RENDER_H: u32 = 512;
const TILE_EXTENT_M: f64 = 1024.0;
// per-channel jitter ceiling and per-image budget. tight but tolerates
// tiny-skia AA noise across patch versions.
const TOLERANCE_CHANNELS: u8 = 2;
const MAX_DIFF_RATIO: f32 = 0.005;

#[tokio::test(flavor = "multi_thread")]
async fn demo_mini_matches_golden() -> Result<()> {
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/parcels-mini");
    let seed_sql = std::fs::read_to_string(fixture_dir.join("seed.sql")).context("read seed.sql")?;
    let yaml_template = std::fs::read_to_string(fixture_dir.join("service.yaml")).context("read service.yaml")?;
    let golden_path = fixture_dir.join("goldens/parcels-cell-0-0.png");

    let password = Alphanumeric.sample_string(&mut rand::thread_rng(), 16);
    let container = GenericImage::new("postgis/postgis", "16-3.4")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_PASSWORD", &password)
        .with_env_var("POSTGRES_USER", "mars")
        .with_env_var("POSTGRES_DB", "mars")
        .start()
        .await
        .context("start postgis container")?;
    let port = container.get_host_port_ipv4(5432).await.context("host port")?;
    let dsn = format!("host=127.0.0.1 port={port} user=mars password={password} dbname=mars");

    if let Err(e) = run_seed(&dsn, &seed_sql).await {
        dump_logs(&container).await;
        return Err(e.context("seed database"));
    }

    let store_dir = TempDir::new().context("store tempdir")?;
    let cache_dir = TempDir::new().context("cache tempdir")?;
    let cfg_dir = TempDir::new().context("cfg tempdir")?;

    let yaml = yaml_template
        .replace("{{DSN}}", &dsn)
        .replace("{{STORE}}", store_dir.path().to_str().unwrap())
        .replace("{{CACHE}}", cache_dir.path().to_str().unwrap());
    let cfg_path = cfg_dir.path().join("mars.yaml");
    std::fs::write(&cfg_path, yaml).context("write rendered yaml")?;

    let cfg: Config = mars_config::load(&cfg_path).context("load fixture config")?;
    mars_config::validate(&cfg, &config_dir(&cfg_path)).context("validate fixture config")?;

    if let Err(e) = run_compile(&cfg).await {
        dump_logs(&container).await;
        return Err(anyhow::anyhow!("compile: {e}"));
    }

    let publisher = FsPublisher::new(store_dir.path()).context("open publisher")?;
    let manifest = publisher
        .current()
        .await
        .context("read current manifest")?
        .context("manifest absent after compile")?;
    assert!(!manifest.layer_artifacts.is_empty(), "no layer artifacts published");

    let stylesheet = build_stylesheet(&cfg);
    let state = RuntimeState::from_config_and_manifest(&cfg, stylesheet, manifest)
        .map_err(|e| anyhow::anyhow!("runtime state: {e}"))?;
    let store = Arc::new(FsStore::new(store_dir.path()).context("open store")?);
    let cache = Arc::new(FsCache::new(cache_dir.path(), u64::MAX).context("open cache")?);
    let runtime = Runtime::from_state(
        Arc::new(state),
        RuntimeDeps {
            store,
            cache,
            renderer: Arc::new(TinySkiaRenderer),
            encoder: Arc::new(TinySkiaEncoder::default()),
            metrics: mars_observability::Metrics::new().expect("metrics"),
        },
    );

    let plan = RenderPlan {
        layers: vec![LayerId::new("parcels")],
        bbox: Bbox::new(0.0, 0.0, TILE_EXTENT_M - 1.0, TILE_EXTENT_M - 1.0),
        width: RENDER_W,
        height: RENDER_H,
        crs: CrsCode::new("EPSG:25832"),
        format: ImageFormat::Png,
    };
    let png_bytes = match runtime.render(&plan).await {
        Ok(b) => b,
        Err(e) => {
            dump_logs(&container).await;
            return Err(anyhow::anyhow!("render: {e}"));
        }
    };

    if std::env::var_os("MARS_GOLDEN_REGENERATE").is_some() {
        if let Some(parent) = golden_path.parent() {
            std::fs::create_dir_all(parent).context("create goldens dir")?;
        }
        std::fs::write(&golden_path, &png_bytes).context("write golden")?;
        eprintln!(
            "MARS_GOLDEN_REGENERATE: wrote {} bytes to {}",
            png_bytes.len(),
            golden_path.display()
        );
        return Ok(());
    }

    let golden = std::fs::read(&golden_path).with_context(|| {
        format!(
            "read golden {} (run with MARS_GOLDEN_REGENERATE=1 to bootstrap)",
            golden_path.display()
        )
    })?;

    let report = diff_pngs(&png_bytes, &golden, TOLERANCE_CHANNELS).map_err(|e| anyhow::anyhow!("diff: {e}"))?;
    eprintln!("image-diff: {report}");
    assert_within_tolerance(&report, MAX_DIFF_RATIO);
    Ok(())
}

async fn run_seed(dsn: &str, sql: &str) -> Result<()> {
    let (client, conn) = connect_with_retry(dsn).await?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::warn!("postgres connection ended: {e}");
        }
    });
    client.batch_execute(sql).await.context("seed sql")?;
    Ok(())
}

async fn connect_with_retry(
    dsn: &str,
) -> Result<(
    tokio_postgres::Client,
    tokio_postgres::Connection<tokio_postgres::Socket, tokio_postgres::tls::NoTlsStream>,
)> {
    let mut last_err = None;
    for _ in 0..30 {
        match tokio_postgres::connect(dsn, NoTls).await {
            Ok(pair) => return Ok(pair),
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    Err(anyhow::anyhow!("postgres connect timed out: {:?}", last_err))
}

async fn run_compile(cfg: &Config) -> Result<()> {
    let pg_cfg = PgConfig {
        dsn: cfg.source.dsn.clone(),
        publication: String::new(),
        slot: String::new(),
    };
    let source = Arc::new(PgSource::connect(pg_cfg).await.context("pg connect")?);
    let store = Arc::new(FsStore::new(cfg.artifacts.store.path.as_deref().unwrap()).context("open compile store")?);
    let publisher =
        Arc::new(FsPublisher::new(cfg.artifacts.store.path.as_deref().unwrap()).context("open compile publisher")?);
    let compiler = Compiler::new(
        CompilerDeps {
            source: source.clone(),
            change_feed: source.clone(),
            leader_lock: source,
            store,
            manifest: publisher,
            metrics: mars_observability::Metrics::new().unwrap(),
        },
        cfg.clone(),
    );
    compiler
        .run(CancellationToken::new())
        .await
        .map_err(|e| anyhow::anyhow!(e))
}

fn build_stylesheet(cfg: &Config) -> Stylesheet {
    let mut ss = Stylesheet::default();
    for (name, entry) in &cfg.styles {
        if let Some(s) = entry.as_geometry() {
            ss.geometry.insert(name.clone(), Arc::new(s.clone()));
        }
    }
    for layer in &cfg.layers {
        for class in &layer.classes {
            if let ClassStyle::Inline(s) = &class.style {
                ss.geometry
                    .insert(format!("{}::{}", layer.name, class.name), Arc::new(s.clone()));
            }
        }
    }
    ss
}

async fn dump_logs<C>(container: &C)
where
    C: ContainerLogs,
{
    container.dump_logs().await;
}

trait ContainerLogs {
    async fn dump_logs(&self);
}

impl<I> ContainerLogs for testcontainers::ContainerAsync<I>
where
    I: testcontainers::Image,
{
    async fn dump_logs(&self) {
        match self.stdout_to_vec().await {
            Ok(b) => eprintln!("--- container stdout ---\n{}", String::from_utf8_lossy(&b)),
            Err(e) => eprintln!("(failed to read stdout: {e})"),
        }
        match self.stderr_to_vec().await {
            Ok(b) => eprintln!("--- container stderr ---\n{}", String::from_utf8_lossy(&b)),
            Err(e) => eprintln!("(failed to read stderr: {e})"),
        }
    }
}

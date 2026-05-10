//! image-diff harness: renders the parcels-mini fixture against a real
//! postgis container and compares the PNG output against checked-in goldens.
//! gated on the `e2e` cargo feature, like `e2e_render.rs`.
//!
//! drives a small matrix of `Case`s against a single shared runtime so the
//! per-case overhead is just a render call. each case has its own golden plus
//! its own per-channel tolerance and per-image diff-ratio budget.
//!
//! regenerate every golden by setting `MARS_GOLDEN_REGENERATE=1`. otherwise
//! any case whose diff exceeds its budget fails the test.

#![cfg(feature = "e2e")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use mars_bin_shared::build_stylesheet;
use mars_compiler::{Compiler, Deps as CompilerDeps};
use mars_config::{Config, config_dir};
use mars_render::{TinySkiaEncoder, TinySkiaRenderer};
use mars_runtime::{Deps as RuntimeDeps, RenderPlan, Runtime, RuntimeState};
use mars_source_postgres::{PgConfig, PgSource};
use mars_store::ManifestStore;
use mars_store_fs::{FsCache, FsPublisher, FsStore};
use mars_types::{Bbox, CrsCode, ImageFormat, LayerId};
use rand::distr::{Alphanumeric, SampleString};
use tempfile::TempDir;
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use tokio_postgres::NoTls;
use tokio_util::sync::CancellationToken;

mod common;
use common::diff_pngs;

/// one case in the diff matrix.
struct Case {
    /// stable id; doubles as the golden filename stem under `goldens/`.
    name: &'static str,
    plan: RenderPlan,
    /// per-channel delta beyond which a pixel is counted as differing.
    tolerance: u8,
    /// maximum fraction of differing pixels tolerated for this case.
    max_diff_ratio: f32,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "parcels-cell-0-0",
            plan: RenderPlan {
                layers: vec![LayerId::new("parcels")],
                bbox: Bbox::new(0.0, 0.0, 1023.0, 1023.0),
                width: 512,
                height: 512,
                crs: CrsCode::new("EPSG:25832"),
                format: ImageFormat::Png,
                scale_pixel_size_m: mars_runtime::OGC_STANDARDIZED_PIXEL_SIZE_M,
            },
            tolerance: 2,
            max_diff_ratio: 0.005,
        },
        // bottom-left quadrant zoom; non-tile-aligned bbox.
        Case {
            name: "parcels-quadrant-sw",
            plan: RenderPlan {
                layers: vec![LayerId::new("parcels")],
                bbox: Bbox::new(0.0, 0.0, 500.0, 500.0),
                width: 256,
                height: 256,
                crs: CrsCode::new("EPSG:25832"),
                format: ImageFormat::Png,
                scale_pixel_size_m: mars_runtime::OGC_STANDARDIZED_PIXEL_SIZE_M,
            },
            tolerance: 2,
            max_diff_ratio: 0.005,
        },
        // top-right quadrant zoom; tighter scale, fewer features in view.
        Case {
            name: "parcels-quadrant-ne",
            plan: RenderPlan {
                layers: vec![LayerId::new("parcels")],
                bbox: Bbox::new(500.0, 500.0, 1000.0, 1000.0),
                width: 256,
                height: 256,
                crs: CrsCode::new("EPSG:25832"),
                format: ImageFormat::Png,
                scale_pixel_size_m: mars_runtime::OGC_STANDARDIZED_PIXEL_SIZE_M,
            },
            tolerance: 2,
            max_diff_ratio: 0.005,
        },
        // labelled cell - same plan as parcels-cell-0-0 but the renderer
        // composites text on top. the budget is loosened to absorb glyph AA.
        Case {
            name: "parcels-cell-0-0-with-labels",
            plan: RenderPlan {
                layers: vec![LayerId::new("parcels")],
                bbox: Bbox::new(0.0, 0.0, 1023.0, 1023.0),
                width: 512,
                height: 512,
                crs: CrsCode::new("EPSG:25832"),
                format: ImageFormat::Png,
                scale_pixel_size_m: mars_runtime::OGC_STANDARDIZED_PIXEL_SIZE_M,
            },
            tolerance: 8,
            max_diff_ratio: 0.02,
        },
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn parcels_mini_matrix() -> Result<()> {
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/parcels-mini");
    let seed_sql = std::fs::read_to_string(fixture_dir.join("seed.sql")).context("read seed.sql")?;
    let yaml_template = std::fs::read_to_string(fixture_dir.join("service.yaml")).context("read service.yaml")?;
    let goldens_dir = fixture_dir.join("goldens");

    let password = Alphanumeric.sample_string(&mut rand::rng(), 16);
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

    let mut cfg: Config = mars_config::load(&cfg_path).context("load fixture config")?;
    mars_config::validate(&mut cfg, &config_dir(&cfg_path)).context("validate fixture config")?;

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
    assert!(!manifest.pages.is_empty(), "no page artifacts published");

    let stylesheet = build_stylesheet(&cfg);
    let state = RuntimeState::from_config_and_manifest(&cfg, stylesheet, manifest)
        .map_err(|e| anyhow::anyhow!("runtime state: {e}"))?;
    let store = Arc::new(FsStore::new(store_dir.path()).context("open store")?);
    let cache = Arc::new(FsCache::new(cache_dir.path(), u64::MAX).context("open cache")?);
    let fonts = Arc::new(mars_runtime::Fonts::with_default());
    let runtime = Runtime::from_state(
        Arc::new(state),
        RuntimeDeps {
            store,
            cache,
            renderer: Arc::new(TinySkiaRenderer::new(fonts.clone())),
            encoder: Arc::new(TinySkiaEncoder::default()),
            metrics: mars_observability::Metrics::new().expect("metrics"),
            fonts,
        },
    );

    let regenerate = std::env::var_os("MARS_GOLDEN_REGENERATE").is_some();
    if regenerate {
        std::fs::create_dir_all(&goldens_dir).context("create goldens dir")?;
    }

    let mut failures = Vec::new();
    for case in cases() {
        let golden_path = goldens_dir.join(format!("{}.png", case.name));
        let png_bytes = match runtime.render(&case.plan).await {
            Ok(b) => b,
            Err(e) => {
                dump_logs(&container).await;
                return Err(anyhow::anyhow!("render case {}: {e}", case.name));
            }
        };

        if regenerate {
            std::fs::write(&golden_path, &png_bytes).with_context(|| format!("write golden for case {}", case.name))?;
            eprintln!(
                "MARS_GOLDEN_REGENERATE: case={} wrote {} bytes to {}",
                case.name,
                png_bytes.len(),
                golden_path.display()
            );
            continue;
        }

        let golden = std::fs::read(&golden_path).with_context(|| {
            format!(
                "read golden {} (run with MARS_GOLDEN_REGENERATE=1 to bootstrap)",
                golden_path.display()
            )
        })?;

        let report = diff_pngs(&png_bytes, &golden, case.tolerance)
            .map_err(|e| anyhow::anyhow!("diff case {}: {e}", case.name))?;
        eprintln!("image-diff: case={} {report}", case.name);
        if report.diff_ratio() > case.max_diff_ratio {
            failures.push(format!(
                "case {}: ratio={:.6} > budget {:.6}; {}",
                case.name,
                report.diff_ratio(),
                case.max_diff_ratio,
                report,
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "image-diff matrix had {} failure(s):\n  - {}",
        failures.len(),
        failures.join("\n  - ")
    );
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
        ..Default::default()
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
        .run_snapshot_once(CancellationToken::new())
        .await
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!(e))
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

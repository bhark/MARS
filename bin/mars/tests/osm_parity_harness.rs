//! osm-parity harness: renders the osm-parity fixture against a real
//! postgis container and compares the output against checked-in goldens
//! captured one-shot from a reference WMS.
//!
//! gated on the `integration` cargo feature. one shared container hosts one
//! compile; each case is a single render call against the resulting runtime.
//!
//! prerequisites:
//!   target/parity-fixtures/osm-parity.sql.gz  - the seed dump, not committed.
//!
//! the goldens are one-shot captures from an independent reference renderer;
//! the harness deliberately offers no in-process regeneration path so a green
//! run cannot mean "MARS agrees with MARS". failing diffs drop actual/golden
//! bytes into `target/parity-output/<case>/` so the divergence is inspectable.

#![cfg(feature = "integration")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
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
    core::{AccessMode, IntoContainerPort, Mount, WaitFor},
    runners::AsyncRunner,
};
use tokio_postgres::NoTls;
use tokio_util::sync::CancellationToken;

mod common;
use common::diff_pngs_with_radius;

// neighborhood radius used by the parity diff. one-renderer-vs-another differs
// at the pixel level by sub-pixel AA edges plus minor positional drift; r=2
// forgives those within (2r+1)x(2r+1) windows while keeping real divergence
// (a feature in one image that has no match nearby in the other) flagged.
const PARITY_DIFF_RADIUS: u32 = 2;

const ALL_LAYERS: &[&str] = &[
    "landuse",
    "water",
    "waterways",
    "roads_minor",
    "roads_major",
    "buildings",
    "boundary",
    "places",
];

struct Case {
    /// stable id; doubles as the golden filename stem under `goldens/`.
    name: &'static str,
    plan: RenderPlan,
    /// per-channel delta beyond which a pixel is counted as differing.
    tolerance: u8,
    /// maximum fraction of differing pixels tolerated for this case.
    max_diff_ratio: f32,
    /// golden filename extension (`png` or `jpg`).
    ext: &'static str,
}

fn layer_ids(names: &[&str]) -> Vec<LayerId> {
    names.iter().map(|n| LayerId::new(*n)).collect()
}

fn cases() -> Vec<Case> {
    let crs_3857 = || CrsCode::new("EPSG:3857");
    let crs_25832 = || CrsCode::new("EPSG:25832");
    let scale_m = mars_runtime::OGC_STANDARDIZED_PIXEL_SIZE_M;

    // budgets calibrated against tiny-skia vs mapserver/AGG rendering with
    // neighborhood radius 2: typical std cases sit at 8-12% differing under
    // that relaxation, reprojection adds another ~2-3%, jpeg is bounded by
    // chroma drift on top, blanks are pixel-exact.
    let std_tol = 8u8;
    let std_ratio = 0.13f32;
    let reproj_tol = 12u8;
    let reproj_ratio = 0.15f32;
    let jpeg_tol = 24u8;
    let jpeg_ratio = 0.05f32;
    let blank_tol = 2u8;
    let blank_ratio = 0.001f32;

    vec![
        Case {
            name: "overview-3857-512",
            plan: RenderPlan {
                layers: layer_ids(ALL_LAYERS),
                bbox: Bbox::new(1_040_000.0, 5_930_000.0, 1_090_000.0, 6_000_000.0),
                width: 512,
                height: 512,
                crs: crs_3857(),
                format: ImageFormat::Png,
                scale_pixel_size_m: scale_m,
            },
            tolerance: std_tol,
            max_diff_ratio: std_ratio,
            ext: "png",
        },
        Case {
            name: "mid-vaduz-3857-512",
            plan: RenderPlan {
                layers: layer_ids(ALL_LAYERS),
                bbox: Bbox::new(1_057_000.0, 5_958_000.0, 1_063_000.0, 5_964_000.0),
                width: 512,
                height: 512,
                crs: crs_3857(),
                format: ImageFormat::Png,
                scale_pixel_size_m: scale_m,
            },
            tolerance: std_tol,
            max_diff_ratio: std_ratio,
            ext: "png",
        },
        Case {
            name: "mid-schaan-3857-512",
            plan: RenderPlan {
                layers: layer_ids(ALL_LAYERS),
                bbox: Bbox::new(1_055_500.0, 5_960_500.0, 1_061_500.0, 5_966_500.0),
                width: 512,
                height: 512,
                crs: crs_3857(),
                format: ImageFormat::Png,
                scale_pixel_size_m: scale_m,
            },
            tolerance: std_tol,
            max_diff_ratio: std_ratio,
            ext: "png",
        },
        Case {
            name: "mid-rural-3857-512",
            plan: RenderPlan {
                layers: layer_ids(ALL_LAYERS),
                bbox: Bbox::new(1_066_000.0, 5_958_000.0, 1_072_000.0, 5_964_000.0),
                width: 512,
                height: 512,
                crs: crs_3857(),
                format: ImageFormat::Png,
                scale_pixel_size_m: scale_m,
            },
            tolerance: std_tol,
            max_diff_ratio: std_ratio,
            ext: "png",
        },
        Case {
            name: "detail-vaduz-3857-512",
            plan: RenderPlan {
                layers: layer_ids(ALL_LAYERS),
                bbox: Bbox::new(1_059_400.0, 5_960_400.0, 1_060_600.0, 5_961_600.0),
                width: 512,
                height: 512,
                crs: crs_3857(),
                format: ImageFormat::Png,
                scale_pixel_size_m: scale_m,
            },
            tolerance: std_tol,
            max_diff_ratio: std_ratio,
            ext: "png",
        },
        Case {
            name: "detail-rural-3857-512",
            plan: RenderPlan {
                layers: layer_ids(ALL_LAYERS),
                bbox: Bbox::new(1_068_400.0, 5_960_400.0, 1_069_600.0, 5_961_600.0),
                width: 512,
                height: 512,
                crs: crs_3857(),
                format: ImageFormat::Png,
                scale_pixel_size_m: scale_m,
            },
            tolerance: std_tol,
            max_diff_ratio: std_ratio,
            ext: "png",
        },
        Case {
            name: "mid-vaduz-25832-512",
            plan: RenderPlan {
                layers: layer_ids(ALL_LAYERS),
                bbox: Bbox::new(536_543.0, 5_217_965.0, 542_543.0, 5_223_965.0),
                width: 512,
                height: 512,
                crs: crs_25832(),
                format: ImageFormat::Png,
                scale_pixel_size_m: scale_m,
            },
            tolerance: reproj_tol,
            max_diff_ratio: reproj_ratio,
            ext: "png",
        },
        Case {
            name: "mid-vaduz-3857-1024",
            plan: RenderPlan {
                layers: layer_ids(ALL_LAYERS),
                bbox: Bbox::new(1_057_000.0, 5_958_000.0, 1_063_000.0, 5_964_000.0),
                width: 1024,
                height: 1024,
                crs: crs_3857(),
                format: ImageFormat::Png,
                scale_pixel_size_m: scale_m,
            },
            tolerance: std_tol,
            max_diff_ratio: std_ratio,
            ext: "png",
        },
        Case {
            name: "mid-vaduz-3857-jpeg",
            plan: RenderPlan {
                layers: layer_ids(ALL_LAYERS),
                bbox: Bbox::new(1_057_000.0, 5_958_000.0, 1_063_000.0, 5_964_000.0),
                width: 512,
                height: 512,
                crs: crs_3857(),
                format: ImageFormat::Jpeg,
                scale_pixel_size_m: scale_m,
            },
            tolerance: jpeg_tol,
            max_diff_ratio: jpeg_ratio,
            ext: "jpg",
        },
        Case {
            name: "mid-vaduz-roads-only",
            plan: RenderPlan {
                layers: layer_ids(&["roads_major", "roads_minor"]),
                bbox: Bbox::new(1_057_000.0, 5_958_000.0, 1_063_000.0, 5_964_000.0),
                width: 512,
                height: 512,
                crs: crs_3857(),
                format: ImageFormat::Png,
                scale_pixel_size_m: scale_m,
            },
            tolerance: std_tol,
            max_diff_ratio: std_ratio,
            ext: "png",
        },
        Case {
            name: "mid-vaduz-landuse-only",
            plan: RenderPlan {
                layers: layer_ids(&["landuse"]),
                bbox: Bbox::new(1_057_000.0, 5_958_000.0, 1_063_000.0, 5_964_000.0),
                width: 512,
                height: 512,
                crs: crs_3857(),
                format: ImageFormat::Png,
                scale_pixel_size_m: scale_m,
            },
            tolerance: std_tol,
            max_diff_ratio: std_ratio,
            ext: "png",
        },
        Case {
            name: "detail-vaduz-buildings-only",
            plan: RenderPlan {
                layers: layer_ids(&["buildings"]),
                bbox: Bbox::new(1_059_400.0, 5_960_400.0, 1_060_600.0, 5_961_600.0),
                width: 512,
                height: 512,
                crs: crs_3857(),
                format: ImageFormat::Png,
                scale_pixel_size_m: scale_m,
            },
            tolerance: std_tol,
            max_diff_ratio: std_ratio,
            ext: "png",
        },
        Case {
            name: "detail-vaduz-roads-only",
            plan: RenderPlan {
                layers: layer_ids(&["roads_major", "roads_minor"]),
                bbox: Bbox::new(1_059_400.0, 5_960_400.0, 1_060_600.0, 5_961_600.0),
                width: 512,
                height: 512,
                crs: crs_3857(),
                format: ImageFormat::Png,
                scale_pixel_size_m: scale_m,
            },
            tolerance: std_tol,
            max_diff_ratio: std_ratio,
            ext: "png",
        },
        Case {
            name: "overview-3857-landuse-only",
            plan: RenderPlan {
                layers: layer_ids(&["landuse"]),
                bbox: Bbox::new(1_040_000.0, 5_930_000.0, 1_090_000.0, 6_000_000.0),
                width: 512,
                height: 512,
                crs: crs_3857(),
                format: ImageFormat::Png,
                scale_pixel_size_m: scale_m,
            },
            // both renderers emit only the page background at this denom
            // because the layer's MAXSCALEDENOM gate excludes it. counts as a
            // tight parity case: only background-fill divergence is tolerated.
            tolerance: blank_tol,
            max_diff_ratio: blank_ratio,
            ext: "png",
        },
        Case {
            name: "overview-3857-boundary-only",
            plan: RenderPlan {
                layers: layer_ids(&["boundary"]),
                bbox: Bbox::new(1_040_000.0, 5_930_000.0, 1_090_000.0, 6_000_000.0),
                width: 512,
                height: 512,
                crs: crs_3857(),
                format: ImageFormat::Png,
                scale_pixel_size_m: scale_m,
            },
            tolerance: std_tol,
            max_diff_ratio: std_ratio,
            ext: "png",
        },
        Case {
            name: "empty-bbox-3857",
            plan: RenderPlan {
                layers: layer_ids(ALL_LAYERS),
                bbox: Bbox::new(900_000.0, 5_900_000.0, 905_000.0, 5_905_000.0),
                width: 512,
                height: 512,
                crs: crs_3857(),
                format: ImageFormat::Png,
                scale_pixel_size_m: scale_m,
            },
            tolerance: blank_tol,
            max_diff_ratio: blank_ratio,
            ext: "png",
        },
    ]
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/osm-parity")
}

fn dump_path() -> PathBuf {
    // workspace target/parity-fixtures/. bin/mars's CARGO_MANIFEST_DIR is
    // bin/mars; the workspace target lives two levels up.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/parity-fixtures/osm-parity.sql.gz")
        .canonicalize()
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/parity-fixtures/osm-parity.sql.gz")
        })
}

fn diff_output_dir(case_name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/parity-output")
        .join(case_name)
}

#[tokio::test(flavor = "multi_thread")]
async fn osm_parity_matrix() -> Result<()> {
    let fixture = fixture_dir();
    let dump = dump_path();
    if !dump.exists() {
        return Err(anyhow::anyhow!(
            "osm-parity dump missing at {}\nproduce it offline (see {}/README.md) before running this harness.",
            dump.display(),
            fixture.display()
        ));
    }

    let yaml_template = std::fs::read_to_string(fixture.join("service.yaml")).context("read service.yaml")?;
    let goldens_dir = fixture.join("goldens");
    let seed_sql = fixture.join("seed.sql");
    let restore_sh = fixture.join("restore.sh");
    let views_sql = fixture.join("02-views.sql");

    let password = Alphanumeric.sample_string(&mut rand::rng(), 16);
    let container = GenericImage::new("postgis/postgis", "18-3.6")
        .with_exposed_port(5432.tcp())
        // postgres logs "ready to accept connections" once during init (local-
        // only) and again after init scripts run. connect_with_retry rides
        // through the gap between the two.
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_mount(
            Mount::bind_mount(seed_sql.to_string_lossy(), "/docker-entrypoint-initdb.d/00-seed.sql")
                .with_access_mode(AccessMode::ReadOnly),
        )
        .with_mount(
            Mount::bind_mount(
                restore_sh.to_string_lossy(),
                "/docker-entrypoint-initdb.d/01-restore.sh",
            )
            .with_access_mode(AccessMode::ReadOnly),
        )
        .with_mount(
            Mount::bind_mount(views_sql.to_string_lossy(), "/docker-entrypoint-initdb.d/02-views.sql")
                .with_access_mode(AccessMode::ReadOnly),
        )
        .with_mount(
            Mount::bind_mount(dump.to_string_lossy(), "/opt/parity-fixture/osm-parity.sql.gz")
                .with_access_mode(AccessMode::ReadOnly),
        )
        .with_env_var("POSTGRES_PASSWORD", &password)
        .with_env_var("POSTGRES_USER", "mars")
        .with_env_var("POSTGRES_DB", "mars")
        .start()
        .await
        .context("start postgis container")?;
    let port = container.get_host_port_ipv4(5432).await.context("host port")?;
    let dsn = format!("host=127.0.0.1 port={port} user=mars password={password} dbname=mars");

    if let Err(e) = wait_for_dump_ready(&dsn).await {
        dump_logs(&container).await;
        return Err(e.context("wait for postgres + dump"));
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
    let images = Arc::new(mars_runtime::images::MutableImageRegistry::new());
    let runtime = Runtime::from_state(
        Arc::new(state),
        RuntimeDeps {
            store,
            cache,
            renderer: Arc::new(TinySkiaRenderer::with_images(fonts.clone(), images.clone())),
            encoder: Arc::new(TinySkiaEncoder::default()),
            metrics: mars_observability::Metrics::new().expect("metrics"),
            fonts,
            images,
            raster_sources: HashMap::new(),
        },
    );

    let mut failures = Vec::new();
    for case in cases() {
        let golden_path = goldens_dir.join(format!("{}.{}", case.name, case.ext));
        let bytes = match runtime.render(&case.plan).await {
            Ok(b) => b,
            Err(e) => {
                dump_logs(&container).await;
                return Err(anyhow::anyhow!("render case {}: {e}", case.name));
            }
        };

        let golden = std::fs::read(&golden_path).with_context(|| format!("read golden {}", golden_path.display()))?;

        match diff_pngs_with_radius(&bytes, &golden, case.tolerance, PARITY_DIFF_RADIUS) {
            Ok(report) => {
                eprintln!("osm-parity: case={} {report}", case.name);
                if report.diff_ratio() > case.max_diff_ratio {
                    save_diff_artifacts(case.name, case.ext, &bytes, &golden);
                    failures.push(format!(
                        "case {}: ratio={:.6} > budget {:.6}; {}",
                        case.name,
                        report.diff_ratio(),
                        case.max_diff_ratio,
                        report,
                    ));
                }
            }
            Err(e) => {
                save_diff_artifacts(case.name, case.ext, &bytes, &golden);
                failures.push(format!("case {}: diff error: {e}", case.name));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "osm-parity matrix had {} failure(s):\n  - {}",
        failures.len(),
        failures.join("\n  - ")
    );
    Ok(())
}

async fn wait_for_dump_ready(dsn: &str) -> Result<()> {
    // postgres init-script restoration on the osm-parity dump takes tens of
    // seconds. retry connect long enough to ride through the post-init
    // listener restart and the actual restore.
    let mut last_err = None;
    for _ in 0..240 {
        match tokio_postgres::connect(dsn, NoTls).await {
            Ok((client, conn)) => {
                tokio::spawn(async move {
                    if let Err(e) = conn.await {
                        tracing::warn!("postgres connection ended: {e}");
                    }
                });
                // gate on the parity views existing - 02-views.sql runs
                // after the COPY blocks land.
                let row: Option<bool> = client
                    .query_one("SELECT to_regclass('public.parity_landuse') IS NOT NULL", &[])
                    .await
                    .ok()
                    .and_then(|r| r.try_get::<_, bool>(0).ok());
                if row == Some(true) {
                    return Ok(());
                }
            }
            Err(e) => last_err = Some(e),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(anyhow::anyhow!(
        "postgres / osm-parity dump did not become ready: {:?}",
        last_err
    ))
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

fn save_diff_artifacts(case: &str, ext: &str, actual: &[u8], golden: &[u8]) {
    let dir = diff_output_dir(case);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = std::fs::write(dir.join(format!("actual.{ext}")), actual);
    let _ = std::fs::write(dir.join(format!("golden.{ext}")), golden);
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

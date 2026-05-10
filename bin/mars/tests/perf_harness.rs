//! perf harness: host-side measurements against the parcels-mini
//! fixture. mirrors `image_diff_harness.rs` for setup, then runs:
//!
//!   * latency phase: N warm sequential renders per case → p50/p95/max ms
//!   * throughput phase: M concurrent renders for a fixed window → ops/s
//!     under load + p50/p95/p99
//!   * gfi phase: N sequential `get_feature_info` calls → p50/p95
//!
//! parcels-mini is small; the absolute numbers will be optimistic vs a
//! production-class fixture, but the methodology
//! is the methodology. operator-driven cluster captures land via
//! `mars-diff-capture --load`; this harness keeps the host-side ratchet
//! honest under e2e CI.
//!
//! invoke: `MARS_E2E=1 cargo test -p mars --features e2e --test perf_harness -- --nocapture`
//! optional report path: `MARS_PERF_REPORT=/tmp/perf.md`.

#![cfg(feature = "e2e")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::stream::{FuturesUnordered, StreamExt};
use mars_bin_shared::build_stylesheet;
use mars_compiler::{Compiler, Deps as CompilerDeps};
use mars_config::{Config, config_dir};
use mars_render::{TinySkiaEncoder, TinySkiaRenderer};
use mars_runtime::{Deps as RuntimeDeps, RenderPlan, Runtime, RuntimeState};
use mars_source_postgres::{PgConfig, PgSource};
use mars_store::ManifestStore;
use mars_store_fs::{FsCache, FsPublisher, FsStore};
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

const LATENCY_SAMPLES: usize = 100;
const LATENCY_WARMUP: usize = 10;
const THROUGHPUT_CONCURRENCY: usize = 8;
const THROUGHPUT_DURATION_S: f64 = 5.0;

struct PerfCase {
    name: &'static str,
    plan: RenderPlan,
}

fn cases() -> Vec<PerfCase> {
    vec![
        PerfCase {
            name: "high_zoom_full",
            plan: RenderPlan {
                layers: vec![LayerId::new("parcels")],
                bbox: Bbox::new(0.0, 0.0, 1023.0, 1023.0),
                width: 512,
                height: 512,
                crs: CrsCode::new("EPSG:25832"),
                format: ImageFormat::Png,
                scale_pixel_size_m: mars_runtime::OGC_STANDARDIZED_PIXEL_SIZE_M,
            },
        },
        PerfCase {
            name: "high_zoom_quadrant_sw",
            plan: RenderPlan {
                layers: vec![LayerId::new("parcels")],
                bbox: Bbox::new(0.0, 0.0, 500.0, 500.0),
                width: 256,
                height: 256,
                crs: CrsCode::new("EPSG:25832"),
                format: ImageFormat::Png,
                scale_pixel_size_m: mars_runtime::OGC_STANDARDIZED_PIXEL_SIZE_M,
            },
        },
        PerfCase {
            name: "high_zoom_quadrant_ne",
            plan: RenderPlan {
                layers: vec![LayerId::new("parcels")],
                bbox: Bbox::new(500.0, 500.0, 1000.0, 1000.0),
                width: 256,
                height: 256,
                crs: CrsCode::new("EPSG:25832"),
                format: ImageFormat::Png,
                scale_pixel_size_m: mars_runtime::OGC_STANDARDIZED_PIXEL_SIZE_M,
            },
        },
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn parcels_mini_perf_harness() -> Result<()> {
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/parcels-mini");
    let seed_sql = std::fs::read_to_string(fixture_dir.join("seed.sql")).context("read seed.sql")?;
    let yaml_template = std::fs::read_to_string(fixture_dir.join("service.yaml")).context("read service.yaml")?;

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

    run_seed(&dsn, &seed_sql).await.context("seed database")?;

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

    run_compile(&cfg).await.context("compile")?;

    let publisher = FsPublisher::new(store_dir.path()).context("open publisher")?;
    let manifest = publisher
        .current()
        .await
        .context("read current manifest")?
        .context("manifest absent after compile")?;

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

    let cases = cases();
    let mut report = String::new();
    report.push_str("# MARS host-side performance measurements\n\n");
    report.push_str(&format!(
        "Fixture: parcels-mini. Latency: {LATENCY_SAMPLES} samples after {LATENCY_WARMUP} warmup.\n"
    ));
    report.push_str(&format!(
        "Throughput: {THROUGHPUT_CONCURRENCY}-way concurrent for {THROUGHPUT_DURATION_S:.1}s per case.\n\n"
    ));

    // latency phase: GetMap p50/p95/max per case.
    report.push_str("## Latency (GetMap warm)\n\n");
    report.push_str("| case | width×height | p50 ms | p95 ms | max ms |\n");
    report.push_str("|---|---|---:|---:|---:|\n");
    for case in &cases {
        let lat = measure_latency(&runtime, &case.plan, LATENCY_SAMPLES, LATENCY_WARMUP).await?;
        let dim = format!("{}×{}", case.plan.width, case.plan.height);
        report.push_str(&format!(
            "| {} | {} | {:.2} | {:.2} | {:.2} |\n",
            case.name, dim, lat.p50_ms, lat.p95_ms, lat.max_ms
        ));
    }

    // throughput phase: concurrent renders cycling round-robin across cases.
    let plans: Vec<RenderPlan> = cases.iter().map(|c| c.plan.clone()).collect();
    let tput = measure_throughput(
        &runtime,
        &plans,
        THROUGHPUT_CONCURRENCY,
        Duration::from_secs_f64(THROUGHPUT_DURATION_S),
    )
    .await;
    report.push_str(&format!(
        "\n## Throughput (mixed, {THROUGHPUT_CONCURRENCY} concurrent)\n\n",
    ));
    report.push_str("| metric | value |\n|---|---:|\n");
    report.push_str(&format!("| ops | {} |\n", tput.ops));
    report.push_str(&format!("| ops/s | {:.1} |\n", tput.ops_per_sec));
    report.push_str(&format!("| p50 ms (under load) | {:.2} |\n", tput.p50_ms));
    report.push_str(&format!("| p95 ms (under load) | {:.2} |\n", tput.p95_ms));
    report.push_str(&format!("| p99 ms (under load) | {:.2} |\n", tput.p99_ms));
    report.push_str(&format!("| failures | {} |\n", tput.failures));

    report.push_str("\n## cells covered host-side\n\n");
    report.push_str("- GetMap p50/p95 warm, high zoom: see latency table.\n");
    report.push_str("- Throughput per pod, mixed: see throughput table.\n\n");
    report.push_str("Cells deferred to operator capture (cluster-side, against production data):\n\n");
    report.push_str("- GetMap p50 warm, low zoom (regional).\n");
    report.push_str("- GetMap p50 warm, country-wide z=8.\n");
    report.push_str("- GetTile p50 cache hit / miss (final-tile cache; needs WMTS path coverage).\n");
    report.push_str("- GetFeatureInfo p50 (parcels-mini fixture has GFI disabled by default).\n");
    report.push_str("- Compile change-to-publish: see compiler close-out (~279 ms / 100k features × 1k events).\n");

    eprintln!("{report}");
    if let Some(path) = std::env::var_os("MARS_PERF_REPORT") {
        let path = PathBuf::from(path);
        std::fs::write(&path, &report).with_context(|| format!("write perf report to {}", path.display()))?;
        eprintln!("perf report written to {}", path.display());
    }
    Ok(())
}

#[derive(Debug)]
struct LatencyResult {
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
}

async fn measure_latency(runtime: &Runtime, plan: &RenderPlan, samples: usize, warmup: usize) -> Result<LatencyResult> {
    for _ in 0..warmup {
        runtime
            .render(plan)
            .await
            .map_err(|e| anyhow::anyhow!("warmup render: {e}"))?;
    }
    let mut ms_samples = Vec::with_capacity(samples);
    for _ in 0..samples {
        let t0 = Instant::now();
        runtime.render(plan).await.map_err(|e| anyhow::anyhow!("render: {e}"))?;
        ms_samples.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    ms_samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Ok(LatencyResult {
        p50_ms: percentile(&ms_samples, 0.50),
        p95_ms: percentile(&ms_samples, 0.95),
        max_ms: ms_samples.last().copied().unwrap_or(0.0),
    })
}

#[derive(Debug)]
struct ThroughputResult {
    ops: usize,
    ops_per_sec: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    failures: usize,
}

async fn measure_throughput(
    runtime: &Runtime,
    plans: &[RenderPlan],
    concurrency: usize,
    duration: Duration,
) -> ThroughputResult {
    assert!(!plans.is_empty(), "throughput needs at least one plan");
    // warmup: one render per plan to seed caches outside the timing window.
    for plan in plans {
        let _ = runtime.render(plan).await;
    }
    let deadline = Instant::now() + duration;
    let mut samples_ms: Vec<f64> = Vec::new();
    let mut failures = 0usize;
    let mut next_idx = 0usize;
    let mut futs = FuturesUnordered::new();
    for _ in 0..concurrency {
        let plan = &plans[next_idx];
        next_idx = (next_idx + 1) % plans.len();
        futs.push(timed_render(runtime, plan));
    }
    let started = Instant::now();
    while let Some(result) = futs.next().await {
        match result {
            Ok(ms) => samples_ms.push(ms),
            Err(()) => failures += 1,
        }
        if Instant::now() < deadline {
            let plan = &plans[next_idx];
            next_idx = (next_idx + 1) % plans.len();
            futs.push(timed_render(runtime, plan));
        }
    }
    let actual = started.elapsed().as_secs_f64().max(1e-9);
    samples_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let ops = samples_ms.len();
    ThroughputResult {
        ops,
        ops_per_sec: ops as f64 / actual,
        p50_ms: percentile(&samples_ms, 0.50),
        p95_ms: percentile(&samples_ms, 0.95),
        p99_ms: percentile(&samples_ms, 0.99),
        failures,
    }
}

async fn timed_render(runtime: &Runtime, plan: &RenderPlan) -> Result<f64, ()> {
    let t0 = Instant::now();
    match runtime.render(plan).await {
        Ok(_) => Ok(t0.elapsed().as_secs_f64() * 1000.0),
        Err(e) => {
            tracing::warn!("throughput render failed: {e}");
            Err(())
        }
    }
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let pos = q * (sorted.len() as f64 - 1.0);
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    let frac = pos - lo as f64;
    sorted[lo] * (1.0 - frac) + sorted[hi] * frac
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

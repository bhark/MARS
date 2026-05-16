//! smoking gun: postgis -> compiler -> FsStore/FsPublisher -> Runtime ->
//! render -> decodable PNG, all in one process.
//!
//! the existing `integration_bootstrap_snapshot.rs` proves the manifest
//! lands and is readable through `ArtifactReader` at the codec level, but
//! never invokes the runtime. this is the only crate-level test that
//! exercises the exact adapter boundary the e2e suite trips on: what one
//! adapter writes the other must read back through the renderer.

#![cfg(feature = "integration")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::SinkExt;
use mars_bin_shared::build_stylesheet;
use mars_compiler::{Compiler, Deps as CompilerDeps, SourceRegistry};
use mars_config::{Config, SourceId, config_dir};
use mars_render::{TinySkiaEncoder, TinySkiaRenderer};
use mars_runtime::{Deps as RuntimeDeps, Fonts, RenderPlan, Runtime, RuntimeState};
use mars_source::{LeaderLock, LeaderLockGuard, SourceError};
use mars_source_postgres::{PgConfig, PgSource};
use mars_store::ManifestStore;
use mars_store_fs::{FsCache, FsPublisher, FsStore};
use mars_test_support::postgis::boot_postgis;
use mars_types::{Bbox, CrsCode, ImageFormat, LayerId};
use rand::SeedableRng;
use rand::rngs::StdRng;
use tempfile::TempDir;
use tokio_postgres::NoTls;
use tokio_util::sync::CancellationToken;

const FEATURE_COUNT: i64 = 5_000;
const PAGE_TARGET_BYTES: u64 = 64 * 1024;

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_compile_then_runtime_renders_decodable_png() -> Result<()> {
    let pg = boot_postgis().await;
    seed_database(&pg.dsn).await.context("seed database")?;

    let store_dir = TempDir::new().context("store tempdir")?;
    let cache_dir = TempDir::new().context("cache tempdir")?;
    let cfg_dir = TempDir::new().context("cfg tempdir")?;
    let cfg_path = cfg_dir.path().join("mars.yaml");
    std::fs::write(
        &cfg_path,
        fixture_yaml(
            &pg.dsn,
            store_dir.path().to_str().unwrap(),
            cache_dir.path().to_str().unwrap(),
        ),
    )
    .context("write fixture yaml")?;
    let mut cfg: Config = mars_config::load(&cfg_path).context("load fixture")?;
    mars_config::validate(&mut cfg, &config_dir(&cfg_path)).context("validate fixture")?;
    let cfg = Arc::new(cfg);

    let source = Arc::new(
        PgSource::connect(PgConfig {
            dsn: pg.dsn.clone(),
            publication: String::new(),
            slot: String::new(),
            ..Default::default()
        })
        .await
        .context("pg connect")?,
    );

    let store_path = cfg.artifacts.store.path.as_deref().unwrap();
    let store = Arc::new(FsStore::new(store_path).context("open store")?);
    let publisher = Arc::new(FsPublisher::new(store_path).context("open publisher")?);

    // compile
    let mut registry = SourceRegistry::new();
    registry.insert(SourceId::new("default"), source.clone());
    let compiler = Compiler::new(
        CompilerDeps {
            sources: Arc::new(registry),
            change_feed: source.clone(),
            leader_lock: Arc::new(AlwaysLeader),
            store: store.clone(),
            manifest: publisher.clone(),
            metrics: mars_observability::Metrics::new().context("metrics")?,
        },
        (*cfg).clone(),
    );
    let cancel = CancellationToken::new();
    let v = compiler
        .run_snapshot_once(cancel.clone())
        .await
        .context("snapshot compile")?;
    assert_eq!(v, 1, "snapshot must publish manifest v1");

    // build runtime over the same store
    let manifest = publisher
        .current()
        .await
        .context("publisher.current")?
        .expect("manifest after snapshot");
    let cache: Arc<dyn mars_store::LocalCache> =
        Arc::new(FsCache::new(cache_dir.path(), 64 * 1024 * 1024).context("open cache")?);
    let fonts = Arc::new(Fonts::with_default());
    let images = Arc::new(mars_runtime::images::MutableImageRegistry::new());
    let runtime = Arc::new(Runtime::empty(RuntimeDeps {
        store: store.clone(),
        cache,
        renderer: Arc::new(TinySkiaRenderer::with_images(fonts.clone(), images.clone())),
        encoder: Arc::new(TinySkiaEncoder::default()),
        metrics: mars_observability::Metrics::new().context("runtime metrics")?,
        fonts,
        images,
        raster_sources: HashMap::new(),
    }));
    let stylesheet = build_stylesheet(&cfg);
    let state = RuntimeState::from_config_and_manifest(&cfg, stylesheet, manifest).context("build runtime state")?;
    runtime.swap_state(Arc::new(state));

    // render a 512x512 PNG over a 100k x 100k window where ~50 of the 5000
    // seeded points land. covers the seeded extent at a zoom where each
    // point is ~2-3 pixels wide.
    let plan = RenderPlan {
        layers: vec![LayerId::new("pts")],
        bbox: Bbox::new(450_000.0, 450_000.0, 550_000.0, 550_000.0),
        width: 512,
        height: 512,
        crs: CrsCode::new("EPSG:25832"),
        format: ImageFormat::Png,
        scale_pixel_size_m: cfg.service.scale_pixel_size_m(),
    };
    let bytes = runtime.render(&plan).await.context("runtime render")?;
    assert!(!bytes.is_empty(), "render returned empty bytes");

    // PNG decode
    let decoder = png::Decoder::new(std::io::Cursor::new(&bytes));
    let mut reader = decoder.read_info().context("png header")?;
    let info = reader.info().clone();
    assert_eq!(info.width, 512);
    assert_eq!(info.height, 512);
    assert_eq!(info.color_type, png::ColorType::Rgba, "expected rgba png");
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let frame = reader.next_frame(&mut buf).context("png frame")?;
    buf.truncate(frame.buffer_size());

    // assert at least one non-transparent pixel - proves features actually
    // rendered through the full pipeline. with 5000 points in a 512x512
    // viewport over [0,1M)^2, at least dozens of pixels should land.
    let opaque_count = buf.chunks_exact(4).filter(|px| px[3] > 0).count();
    assert!(
        opaque_count > 0,
        "render produced fully transparent image: opaque_count={opaque_count}"
    );

    Ok(())
}

async fn seed_database(dsn: &str) -> Result<()> {
    let (client, conn) = retry_connect(dsn).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
        .batch_execute(
            "CREATE EXTENSION IF NOT EXISTS postgis;\
             CREATE SCHEMA bootstrap;\
             CREATE TABLE bootstrap.points (\
                gid INT8 PRIMARY KEY,\
                name TEXT NOT NULL,\
                geom geometry(Point, 25832) NOT NULL\
             );",
        )
        .await
        .context("schema/seed")?;

    let copy_sql = "COPY bootstrap.points (gid, name, geom) FROM STDIN WITH (FORMAT csv)";
    let sink = client.copy_in(copy_sql).await.context("copy_in")?;
    futures_util::pin_mut!(sink);
    let mut buf = String::with_capacity(64 * 1024);
    let mut rng = StdRng::seed_from_u64(0xC1_5A_71_D5);
    use rand::RngExt;
    let mut sink_unpinned = sink;
    for i in 0..FEATURE_COUNT {
        let x: f64 = rng.random_range(0.0..1_000_000.0);
        let y: f64 = rng.random_range(0.0..1_000_000.0);
        let ewkb = ewkb_point_hex(25832, x, y);
        buf.push_str(&format!("{i},name-{i},{ewkb}\n"));
        if buf.len() > 32 * 1024 {
            sink_unpinned
                .send(bytes::Bytes::from(std::mem::take(&mut buf)))
                .await
                .context("copy send")?;
        }
    }
    if !buf.is_empty() {
        sink_unpinned
            .send(bytes::Bytes::from(std::mem::take(&mut buf)))
            .await
            .context("copy tail send")?;
    }
    let rows = sink_unpinned.finish().await.context("copy finish")?;
    assert_eq!(rows, FEATURE_COUNT as u64);
    Ok(())
}

async fn retry_connect(
    dsn: &str,
) -> Result<(
    tokio_postgres::Client,
    tokio_postgres::Connection<tokio_postgres::Socket, tokio_postgres::tls::NoTlsStream>,
)> {
    for _ in 0..30 {
        match tokio_postgres::connect(dsn, NoTls).await {
            Ok(pair) => return Ok(pair),
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
    Err(anyhow::anyhow!("postgres connect timed out"))
}

fn ewkb_point_hex(srid: u32, x: f64, y: f64) -> String {
    let mut bytes = Vec::with_capacity(25);
    bytes.push(0x01);
    bytes.extend_from_slice(&(1u32 | 0x2000_0000).to_le_bytes());
    bytes.extend_from_slice(&srid.to_le_bytes());
    bytes.extend_from_slice(&x.to_le_bytes());
    bytes.extend_from_slice(&y.to_le_bytes());
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02X}"));
    }
    s
}

fn fixture_yaml(dsn_kv: &str, store_path: &str, cache_path: &str) -> String {
    format!(
        r##"service:
  name: smoking-gun
  title: "Smoking gun"
  abstract: "compile+render integration"
  contact_email: ops@example.org

source:
  type: postgis
  dsn: "{dsn_kv}"
  native_crs: EPSG:25832

artifacts:
  store:
    type: fs
    path: {store_path}
  cache:
    path: {cache_path}
    max_size: 64MiB
    eviction: lru

scales:
  bands:
    - {{ name: hi, max_denom_exclusive: 50000000 }}

cells:
  grid: regular
  origin: [0, 0]
  size_per_band:
    hi: 1024m
  extent:
    min_x: 0
    min_y: 0
    max_x: 1000000
    max_y: 1000000

interfaces:
  wms:
    enabled: true
    versions: ["1.3.0"]
    formats: ["image/png"]

reprojection:
  allowlist: [EPSG:25832]

styles:
  cls_a:
    type: point
    fill: "#dc322f"
    stroke: "#000000"
    stroke_width: 0.5
    marker: {{ kind: circle, size: 6.0 }}

layers:
  - name: pts
    title: "Points"
    type: point
    sources:
      - band: hi
        from: bootstrap.points
        geometry_column: geom
        id_column: gid
        attributes: [name]
        page_size_target_bytes: {PAGE_TARGET_BYTES}
    classes:
      - name: a
        title: "A"
        style: {{ type: ref, name: cls_a }}

compiler:
  window: 5min

observability:
  log_level: info
  log_format: text
"##
    )
}

struct AlwaysLeader;
#[async_trait::async_trait]
impl LeaderLock for AlwaysLeader {
    async fn try_acquire(&self, _key: i64) -> Result<Option<Box<dyn LeaderLockGuard>>, SourceError> {
        Ok(Some(Box::new(NopGuard)))
    }
}
#[derive(Debug)]
struct NopGuard;
impl LeaderLockGuard for NopGuard {}

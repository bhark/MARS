//! garage variant of the smoking gun: postgis -> compiler -> S3Store /
//! S3Publisher (Garage) -> Runtime -> render. exercises the
//! `conditional_put = disabled` + `allow_non_atomic_publish = true`
//! codepath that self-hosted MARS deployments take in production.
//!
//! the SeaweedFS path is the simpler image on paper but `server -s3` is
//! broken upstream (see `mars_test_support::garage` for the rationale).

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
use mars_store_fs::FsCache;
use mars_store_s3::{S3Config, S3Publisher, S3Store};
use mars_test_support::garage::boot_garage;
use mars_test_support::postgis::boot_postgis;
use mars_types::{Bbox, CrsCode, ImageFormat, LayerId};
use rand::SeedableRng;
use rand::rngs::StdRng;
use tempfile::TempDir;
use tokio_postgres::NoTls;
use tokio_util::sync::CancellationToken;

const FEATURE_COUNT: i64 = 1_000;
const PAGE_TARGET_BYTES: u64 = 64 * 1024;

#[tokio::test(flavor = "multi_thread")]
async fn garage_compile_then_runtime_renders_decodable_png() -> Result<()> {
    let pg = boot_postgis().await;
    let garage = boot_garage().await;
    seed_database(&pg.dsn).await.context("seed database")?;

    let cache_dir = TempDir::new().context("cache tempdir")?;
    let cfg_dir = TempDir::new().context("cfg tempdir")?;
    let cfg_path = cfg_dir.path().join("mars.yaml");
    // mars-config currently accepts `type: fs` for the YAML store block; the
    // s3 store is wired programmatically below, bypassing the yaml shape so
    // we don't need to model the s3 section in the test fixture.
    std::fs::write(
        &cfg_path,
        fixture_yaml(&pg.dsn, "/dev/null", cache_dir.path().to_str().unwrap()),
    )
    .context("write fixture yaml")?;
    let mut cfg: Config = mars_config::load(&cfg_path).context("load fixture")?;
    mars_config::validate(&mut cfg, &config_dir(&cfg_path)).context("validate fixture")?;
    let cfg = Arc::new(cfg);

    // garage s3: conditional_put disabled + allow_non_atomic_publish true.
    let s3_cfg = S3Config {
        endpoint: Some(garage.endpoint.clone()),
        region: garage.region.clone(),
        bucket: garage.bucket.clone(),
        prefix: "mars".into(),
        access_key_id: Some(garage.access_key.clone()),
        secret_access_key: Some(garage.secret_key.clone()),
        allow_http: true,
        allow_non_atomic_publish: true,
        conditional_put: Some("disabled".into()),
    };
    let store = Arc::new(S3Store::from_config(&s3_cfg).context("s3 store")?);
    let publisher = Arc::new(S3Publisher::from_store(&store).with_allow_non_atomic_publish(true));

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
    let v = compiler
        .run_snapshot_once(CancellationToken::new())
        .await
        .context("snapshot compile")?;
    assert_eq!(v, 1);

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

    let plan = RenderPlan {
        layers: vec![LayerId::new("pts")],
        bbox: Bbox::new(450_000.0, 450_000.0, 550_000.0, 550_000.0),
        width: 256,
        height: 256,
        crs: CrsCode::new("EPSG:25832"),
        format: ImageFormat::Png,
        scale_pixel_size_m: cfg.service.scale_pixel_size_m(),
    };
    let bytes = runtime.render(&plan).await.context("runtime render")?;
    assert!(!bytes.is_empty());

    let decoder = png::Decoder::new(std::io::Cursor::new(&bytes));
    let mut reader = decoder.read_info().context("png header")?;
    let info = reader.info().clone();
    assert_eq!(info.width, 256);
    assert_eq!(info.height, 256);
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let frame = reader.next_frame(&mut buf).context("png frame")?;
    buf.truncate(frame.buffer_size());

    let opaque_count = buf.chunks_exact(4).filter(|px| px[3] > 0).count();
    assert!(
        opaque_count > 0,
        "garage variant produced fully transparent image: opaque_count={opaque_count}"
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
    sink_unpinned.finish().await.context("copy finish")?;
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
  name: smoking-gun-garage
  title: "smoking gun (garage)"
  abstract: "compile+render via garage s3"
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

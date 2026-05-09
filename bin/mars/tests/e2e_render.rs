//! end-to-end smoke test: compile a real postgis snapshot, then render a tile
//! through the runtime and verify the PNG. gated behind the `e2e` feature so
//! plain `cargo test` never pulls docker into the test set.

#![cfg(feature = "e2e")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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
use rand::distributions::{Alphanumeric, DistString};
use tempfile::TempDir;
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use tokio_postgres::NoTls;
use tokio_util::sync::CancellationToken;

const RENDER_PX: u32 = 256;
const CELL_SIZE_M: f64 = 1024.0;
// fill colours chosen distinct from white background and from each other
const FILL_A: (u8, u8, u8) = (220, 50, 47); // red-ish
const FILL_B: (u8, u8, u8) = (38, 139, 210); // blue-ish
const FILL_C: (u8, u8, u8) = (46, 204, 113); // green-ish

#[tokio::test(flavor = "multi_thread")]
async fn end_to_end_compile_and_render() -> Result<()> {
    // start postgis
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

    if let Err(e) = setup_database(&dsn).await {
        dump_logs(&container).await;
        return Err(e.context("setup database"));
    }

    // tempdirs for store / cache
    let store_dir = TempDir::new().context("store tempdir")?;
    let cache_dir = TempDir::new().context("cache tempdir")?;

    // write fixture yaml with overrides
    let cfg_dir = TempDir::new().context("cfg tempdir")?;
    let cfg_path = cfg_dir.path().join("mars.yaml");
    let yaml = render_fixture_yaml(
        &dsn,
        store_dir.path().to_str().unwrap(),
        cache_dir.path().to_str().unwrap(),
    );
    std::fs::write(&cfg_path, yaml).context("write fixture yaml")?;

    let mut cfg: Config = mars_config::load(&cfg_path).context("load fixture")?;
    mars_config::validate(&mut cfg, &config_dir(&cfg_path)).context("validate fixture")?;

    // compile snapshot
    let compile_result = run_compile(&cfg).await;
    if let Err(e) = &compile_result {
        dump_logs(&container).await;
        return Err(anyhow::anyhow!("compile: {e}"));
    }

    // verify manifest was published and load body
    let publisher = FsPublisher::new(store_dir.path()).context("open publisher")?;
    let manifest = publisher
        .current()
        .await
        .context("read current manifest")?
        .context("manifest absent")?;
    assert_eq!(manifest.version, 1, "expected manifest v1");
    assert!(!manifest.bindings.is_empty(), "no bindings");
    assert!(!manifest.pages.is_empty(), "no pages");

    // build runtime state
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

    // render plan: covers cells (0,0) and (1,0) at the origin.
    // polys has data in (0,0) but is empty in (1,0).
    // sparse_polys has data in (1,0) but is empty in (0,0).
    // this exercises the empty-marker tombstone path end-to-end.
    let plan = RenderPlan {
        layers: vec![LayerId::new("polys"), LayerId::new("sparse_polys")],
        bbox: Bbox::new(0.0, 0.0, CELL_SIZE_M * 2.0 - 1.0, CELL_SIZE_M - 1.0),
        width: RENDER_PX,
        height: RENDER_PX / 2,
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

    // decode + assert
    let decoder = png::Decoder::new(std::io::Cursor::new(&png_bytes));
    let mut reader = decoder.read_info()?;
    let info = reader.info().clone();
    assert_eq!(info.width, RENDER_PX);
    assert_eq!(info.height, RENDER_PX / 2);
    let mut buf = vec![0u8; reader.output_buffer_size().context("png output buffer too large")?];
    let frame = reader.next_frame(&mut buf)?;
    let pixels = &buf[..frame.buffer_size()];
    assert!(
        has_colour_close(pixels, info.color_type, FILL_A),
        "no class-A fill pixels in rendered tile"
    );
    assert!(
        has_colour_close(pixels, info.color_type, FILL_B),
        "no class-B fill pixels in rendered tile"
    );
    assert!(
        has_colour_close(pixels, info.color_type, FILL_C),
        "no sparse_polys fill pixels in rendered tile"
    );

    Ok(())
}

async fn setup_database(dsn: &str) -> Result<()> {
    // small wait for the cluster to actually accept connections; testcontainers
    // signals readiness on stderr but the first TCP connect occasionally still
    // races against role creation.
    let (client, conn) = connect_with_retry(dsn).await?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::warn!("postgres connection ended: {e}");
        }
    });
    client
        .batch_execute(
            "CREATE EXTENSION IF NOT EXISTS postgis;
             CREATE SCHEMA mars_e2e;
             CREATE TABLE mars_e2e.polys (
                gid INT4 PRIMARY KEY,
                klass TEXT NOT NULL,
                geom geometry(Polygon, 25832) NOT NULL
             );
             CREATE TABLE mars_e2e.sparse_polys (
                gid INT4 PRIMARY KEY,
                klass TEXT NOT NULL,
                geom geometry(Polygon, 25832) NOT NULL
             );
             INSERT INTO mars_e2e.polys (gid, klass, geom) VALUES
                (1, 'a', ST_GeomFromText('POLYGON((100 100, 200 100, 200 200, 100 200, 100 100))', 25832)),
                (2, 'a', ST_GeomFromText('POLYGON((300 100, 400 100, 400 200, 300 200, 300 100))', 25832)),
                (3, 'a', ST_GeomFromText('POLYGON((500 100, 600 100, 600 200, 500 200, 500 100))', 25832)),
                (4, 'b', ST_GeomFromText('POLYGON((100 400, 200 400, 200 500, 100 500, 100 400))', 25832)),
                (5, 'b', ST_GeomFromText('POLYGON((300 400, 400 400, 400 500, 300 500, 300 400))', 25832));
             -- sparse_polys only in cell (1,0) [1024,0]-[2048,1024]; cell (0,0) is a gap
             INSERT INTO mars_e2e.sparse_polys (gid, klass, geom) VALUES
                (1, 'a', ST_GeomFromText('POLYGON((1124 100, 1224 100, 1224 200, 1124 200, 1124 100))', 25832)),
                (2, 'b', ST_GeomFromText('POLYGON((1324 100, 1424 100, 1424 200, 1324 200, 1324 100))', 25832));",
        )
        .await
        .context("schema/seed")?;
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

fn render_fixture_yaml(dsn_kv: &str, store_path: &str, cache_path: &str) -> String {
    // the runtime side wants a `postgres://` URL through PgConfig; tokio-postgres
    // accepts both kv and url syntax, so the kv DSN is fine here too.
    let (a_r, a_g, a_b) = FILL_A;
    let (b_r, b_g, b_b) = FILL_B;
    let (c_r, c_g, c_b) = FILL_C;
    let a_hex = format!("{a_r:02x}{a_g:02x}{a_b:02x}");
    let b_hex = format!("{b_r:02x}{b_g:02x}{b_b:02x}");
    let c_hex = format!("{c_r:02x}{c_g:02x}{c_b:02x}");
    format!(
        r##"service:
  name: e2e
  title: "E2E"
  abstract: "e2e fixture"
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
    - {{ name: hi, max_denom_exclusive: 50000 }}

cells:
  grid: regular
  origin: [0, 0]
  size_per_band:
    hi: 1024m
  extent:
    min_x: 0
    min_y: 0
    max_x: 2048
    max_y: 1024

interfaces:
  wms:
    enabled: true
    versions: ["1.3.0"]
    formats: ["image/png"]

reprojection:
  allowlist: [EPSG:25832]

styles:
  cls_a:
    type: polygon
    fill: "#{a_hex}"
    stroke: "#000000"
    stroke_width: 0.5
  cls_b:
    type: polygon
    fill: "#{b_hex}"
    stroke: "#000000"
    stroke_width: 0.5
  cls_c:
    type: polygon
    fill: "#{c_hex}"
    stroke: "#000000"
    stroke_width: 0.5

layers:
  - name: polys
    title: "Polys"
    type: polygon
    sources:
      - band: hi
        from: mars_e2e.polys
        geometry_column: geom
        id_column: gid
        attributes: [klass]
    classes:
      - name: a
        title: "A"
        when: "klass = 'a'"
        style: {{ type: ref, name: cls_a }}
      - name: b
        title: "B"
        when: "klass = 'b'"
        style: {{ type: ref, name: cls_b }}
  - name: sparse_polys
    title: "Sparse Polys"
    type: polygon
    sources:
      - band: hi
        from: mars_e2e.sparse_polys
        geometry_column: geom
        id_column: gid
        attributes: [klass]
    classes:
      - name: a
        title: "A"
        when: "klass = 'a'"
        style: {{ type: ref, name: cls_c }}
      - name: b
        title: "B"
        when: "klass = 'b'"
        style: {{ type: ref, name: cls_c }}

observability:
  log_level: info
  log_format: text
"##
    )
}

fn has_colour_close(pixels: &[u8], color_type: png::ColorType, target: (u8, u8, u8)) -> bool {
    let stride = match color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        _ => return false,
    };
    let (tr, tg, tb) = (i32::from(target.0), i32::from(target.1), i32::from(target.2));
    pixels.chunks_exact(stride).any(|px| {
        let dr = i32::from(px[0]) - tr;
        let dg = i32::from(px[1]) - tg;
        let db = i32::from(px[2]) - tb;
        // generous tolerance; tiny-skia AA blends edge pixels with the background
        dr.abs() <= 20 && dg.abs() <= 20 && db.abs() <= 20
    })
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

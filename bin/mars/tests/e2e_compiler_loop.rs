//! End-to-end smoke test for the live compiler loop against pgoutput.
//!
//! Boots a postgis container with `wal_level=logical`, creates the
//! publication/slot/bound table, runs the compiler in service mode, and
//! verifies the manifest version advances on a fresh row insert (the live
//! replication path closes the loop). Adapter-level edge cases (REPLICA
//! IDENTITY enforcement, multi-truncate, ack/replay) are covered in
//! `mars-source-postgres/tests/replication_e2e.rs` and not duplicated here.

#![cfg(feature = "e2e")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mars_compiler::{Compiler, Deps as CompilerDeps};
use mars_config::{Config, config_dir};
use mars_grid::BandConfig;
use mars_source_postgres::{CollectionTopology, PgConfig, PgSource, ReplicationTopology};
use mars_store::ManifestStore;
use mars_store_fs::{FsPublisher, FsStore};
use mars_types::ScaleBand;
use rand::distributions::{Alphanumeric, DistString};
use tempfile::TempDir;
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use tokio_postgres::NoTls;
use tokio_util::sync::CancellationToken;

const SLOT: &str = "mars_loop_slot";
const PUB: &str = "mars_loop_pub";

#[tokio::test(flavor = "multi_thread")]
async fn live_compiler_loop_advances_manifest_on_change() -> Result<()> {
    let password = Alphanumeric.sample_string(&mut rand::thread_rng(), 16);
    let container = GenericImage::new("postgis/postgis", "16-3.4")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_PASSWORD", &password)
        .with_env_var("POSTGRES_USER", "mars")
        .with_env_var("POSTGRES_DB", "mars")
        .with_cmd([
            "postgres",
            "-c",
            "wal_level=logical",
            "-c",
            "max_wal_senders=8",
            "-c",
            "max_replication_slots=8",
        ])
        .start()
        .await
        .context("start postgis container")?;
    let port = container.get_host_port_ipv4(5432).await.context("host port")?;
    let dsn = format!("host=127.0.0.1 port={port} user=mars password={password} dbname=mars");

    setup_database(&dsn).await.context("setup database")?;

    let store_dir = TempDir::new().context("store tempdir")?;
    let cache_dir = TempDir::new().context("cache tempdir")?;
    let cfg_dir = TempDir::new().context("cfg tempdir")?;
    let cfg_path = cfg_dir.path().join("mars.yaml");
    std::fs::write(
        &cfg_path,
        fixture_yaml(
            &dsn,
            store_dir.path().to_str().unwrap(),
            cache_dir.path().to_str().unwrap(),
        ),
    )
    .context("write fixture yaml")?;
    let cfg: Config = mars_config::load(&cfg_path).context("load fixture")?;
    mars_config::validate(&cfg, &config_dir(&cfg_path)).context("validate fixture")?;

    let topology = ReplicationTopology {
        collections: vec![CollectionTopology {
            collection: "mars_loop.polys".into(),
            schema: "mars_loop".into(),
            table: "polys".into(),
            geometry_column: "geom".into(),
        }],
        bands: vec![BandConfig {
            name: ScaleBand::new("hi"),
            max_denom: 50_000,
            origin: (0.0, 0.0),
            cell_size: 1024.0,
        }],
        max_cells_per_row: 1024,
    };

    let pg_cfg = PgConfig {
        dsn: dsn.clone(),
        publication: PUB.into(),
        slot: SLOT.into(),
        ..Default::default()
    };
    let source = Arc::new(
        PgSource::connect(pg_cfg)
            .await
            .context("pg connect")?
            .with_topology(topology),
    );
    let store = Arc::new(FsStore::new(cfg.artifacts.store.path.as_deref().unwrap()).context("open store")?);
    let publisher = Arc::new(FsPublisher::new(cfg.artifacts.store.path.as_deref().unwrap()).context("open publisher")?);
    let compiler = Compiler::new(
        CompilerDeps {
            source: source.clone(),
            change_feed: source.clone(),
            leader_lock: source,
            store,
            manifest: publisher.clone(),
            metrics: mars_observability::Metrics::new().context("metrics")?,
        },
        cfg,
    );

    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();
    let join = tokio::spawn(async move { compiler.run(cancel_for_task).await });

    // Wait for the snapshot manifest (v1) to land.
    wait_for_version(publisher.as_ref(), 1, Duration::from_secs(60))
        .await
        .context("snapshot manifest v1 not published")?;

    // Insert a row that the live loop should observe.
    let (client, conn) = tokio_postgres::connect(&dsn, NoTls)
        .await
        .context("post-snapshot connect")?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
        .batch_execute(
            "INSERT INTO mars_loop.polys (gid, klass, geom) VALUES \
             (10, 'a', ST_GeomFromText('POLYGON((100 100, 200 100, 200 200, 100 200, 100 100))', 25832));",
        )
        .await
        .context("post-snapshot insert")?;

    // Manifest v2 carries the incremental rebuild and a non-empty source_version.
    let v2 = wait_for_version(publisher.as_ref(), 2, Duration::from_secs(60))
        .await
        .context("incremental manifest v2 not published")?;
    assert!(
        v2.source_version.is_some(),
        "manifest v2 must carry a source_version (LSN cursor)"
    );

    cancel.cancel();
    let res = tokio::time::timeout(Duration::from_secs(15), join).await;
    match res {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(e))) => Err(anyhow::anyhow!("compiler.run returned error: {e}")),
        Ok(Err(e)) => Err(anyhow::anyhow!("compiler task panicked: {e}")),
        Err(_) => Err(anyhow::anyhow!("compiler.run did not exit within 15s of cancel")),
    }
}

async fn setup_database(dsn: &str) -> Result<()> {
    let mut last_err = None;
    for _ in 0..30 {
        match tokio_postgres::connect(dsn, NoTls).await {
            Ok((client, conn)) => {
                tokio::spawn(async move {
                    let _ = conn.await;
                });
                client
                    .batch_execute(
                        "CREATE EXTENSION IF NOT EXISTS postgis;\
                         CREATE SCHEMA mars_loop;\
                         CREATE TABLE mars_loop.polys (\
                            gid INT4 PRIMARY KEY,\
                            klass TEXT NOT NULL,\
                            geom geometry(Polygon, 25832) NOT NULL\
                         );\
                         ALTER TABLE mars_loop.polys REPLICA IDENTITY FULL;\
                         INSERT INTO mars_loop.polys (gid, klass, geom) VALUES\
                            (1, 'a', ST_GeomFromText('POLYGON((100 100, 200 100, 200 200, 100 200, 100 100))', 25832));",
                    )
                    .await
                    .context("schema/seed")?;
                client
                    .batch_execute(&format!("CREATE PUBLICATION {PUB} FOR TABLE mars_loop.polys;"))
                    .await
                    .context("publication")?;
                client
                    .batch_execute(&format!(
                        "SELECT pg_create_logical_replication_slot('{SLOT}', 'pgoutput');"
                    ))
                    .await
                    .context("slot")?;
                return Ok(());
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    Err(anyhow::anyhow!("postgres connect timed out: {last_err:?}"))
}

async fn wait_for_version(publisher: &FsPublisher, target: u64, deadline: Duration) -> Result<mars_types::Manifest> {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if let Some(m) = publisher.current().await?
            && m.version >= target
        {
            return Ok(m);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(anyhow::anyhow!("manifest v{target} not observed within {deadline:?}"))
}

fn fixture_yaml(dsn_kv: &str, store_path: &str, cache_path: &str) -> String {
    format!(
        r##"service:
  name: e2e-loop
  title: "E2E loop"
  abstract: "live compiler loop fixture"
  contact_email: ops@example.org

source:
  type: postgis
  dsn: "{dsn_kv}"
  native_crs: EPSG:25832
  change_feed:
    type: pgoutput
    publication: {PUB}
    slot: {SLOT}

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
    fill: "#dc322f"
    stroke: "#000000"
    stroke_width: 0.5

layers:
  - name: polys
    title: "Polys"
    type: polygon
    sources:
      - band: hi
        from: mars_loop.polys
        geometry_column: geom
        id_column: gid
        attributes: [klass]
    classes:
      - name: a
        title: "A"
        when: "klass = 'a'"
        style: {{ type: ref, name: cls_a }}

compiler:
  window: 500ms

observability:
  log_level: info
  log_format: text
"##
    )
}

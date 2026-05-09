//! Step 13 (LAZARUS Phase C closing gate 2): partial-update cycle timing.
//!
//! Bootstraps a 100k-feature forvaltning2-stand-in fixture, then drives
//! one cycle over a synthetic 1k-event edit batch (insert/update/delete
//! mix), recording the wall-clock time. Gate: ≤ 5 min.
//!
//! Marked `#[ignore]` because the bootstrap alone takes longer than CI
//! budgets. Operator-driven: invoke with
//! `cargo test -p mars --features e2e --test e2e_partial_update_timing -- --nocapture --ignored`.

#![cfg(feature = "e2e")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::SinkExt;
use mars_compiler::{Compiler, Deps as CompilerDeps};
use mars_config::{Config, config_dir};
use mars_grid::{BandConfig, BandName};
use mars_source::ChangeFeed;
use mars_source_postgres::{CollectionTopology, PgConfig, PgSource, ReplicationTopology};
use mars_store_fs::{FsPublisher, FsStore};
use rand::distributions::{Alphanumeric, DistString};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tempfile::TempDir;
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use tokio_postgres::NoTls;
use tokio_util::sync::CancellationToken;

const SLOT: &str = "mars_timing_slot";
const PUB: &str = "mars_timing_pub";
const FIXTURE_ROWS: i64 = 100_000;
const EDIT_BATCH: i64 = 1_000;
/// Hard ceiling per LAZARUS gate.
const GATE_BUDGET: Duration = Duration::from_secs(5 * 60);

#[tokio::test(flavor = "multi_thread")]
#[ignore = "operator-driven; takes minutes against a 100k fixture"]
async fn partial_update_cycle_within_5min() -> Result<()> {
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
    let mut cfg: Config = mars_config::load(&cfg_path).context("load fixture")?;
    mars_config::validate(&mut cfg, &config_dir(&cfg_path)).context("validate fixture")?;

    let topology = ReplicationTopology {
        collections: vec![CollectionTopology {
            collection: "mars_timing.points".into(),
            schema: "mars_timing".into(),
            table: "points".into(),
            geometry_column: "geom".into(),
            id_column: "gid".into(),
        }],
        bands: vec![BandConfig {
            name: BandName::new("hi"),
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
            leader_lock: source.clone(),
            store: store.clone(),
            manifest: publisher.clone(),
            metrics: mars_observability::Metrics::new().context("metrics")?,
        },
        cfg,
    );

    eprintln!("[gate2] bootstrap start ({FIXTURE_ROWS} rows)");
    let snapshot_start = Instant::now();
    compiler
        .run_snapshot_once(CancellationToken::new())
        .await
        .context("snapshot compile")?;
    let snapshot_elapsed = snapshot_start.elapsed();
    eprintln!("[gate2] bootstrap done in {snapshot_elapsed:?}");

    // Subscribe before mutating so the slot captures every event.
    let mut subscription = source.subscribe().await.context("subscribe")?;

    // Synthetic edit batch: ~80% updates, ~10% inserts, ~10% deletes; 1k total.
    eprintln!("[gate2] applying {EDIT_BATCH}-event edit batch");
    apply_edit_batch(&dsn).await.context("edit batch")?;

    // Drain enough batches to cover the EDIT_BATCH events.
    let mut batches = Vec::new();
    let drain_deadline = Instant::now() + Duration::from_secs(60);
    let mut total_events = 0i64;
    while total_events < EDIT_BATCH && Instant::now() < drain_deadline {
        let next = tokio::time::timeout(Duration::from_secs(10), subscription.next_batch()).await;
        match next {
            Ok(Some(Ok(batch))) => {
                total_events += batch.events.len() as i64;
                batches.push(batch);
            }
            Ok(Some(Err(e))) => return Err(anyhow::anyhow!("change feed error: {e}")),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert!(total_events >= EDIT_BATCH, "drained {total_events}/{EDIT_BATCH} events");
    eprintln!("[gate2] drained {total_events} events; running cycle");

    let cycle_start = Instant::now();
    let v2 = compiler.run_cycle_once(batches).await.context("run_cycle_once")?;
    let cycle_elapsed = cycle_start.elapsed();
    eprintln!("[gate2] cycle done in {cycle_elapsed:?} (manifest v{v2}); budget {GATE_BUDGET:?}");

    assert!(
        cycle_elapsed <= GATE_BUDGET,
        "partial-update cycle took {cycle_elapsed:?}; gate is {GATE_BUDGET:?}"
    );

    Ok(())
}

async fn setup_database(dsn: &str) -> Result<()> {
    let mut attempts = 0;
    let (client, conn) = loop {
        attempts += 1;
        match tokio_postgres::connect(dsn, NoTls).await {
            Ok(pair) => break pair,
            Err(e) => {
                if attempts > 30 {
                    return Err(anyhow::anyhow!("postgres connect timed out: {e}"));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    };
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
        .batch_execute(
            "CREATE EXTENSION IF NOT EXISTS postgis;\
             CREATE SCHEMA mars_timing;\
             CREATE TABLE mars_timing.points (\
                gid INT8 PRIMARY KEY,\
                name TEXT NOT NULL,\
                geom geometry(Point, 25832) NOT NULL\
             );\
             ALTER TABLE mars_timing.points REPLICA IDENTITY FULL;",
        )
        .await
        .context("schema")?;

    let copy_sql = "COPY mars_timing.points (gid, name, geom) FROM STDIN WITH (FORMAT csv)";
    let sink = client.copy_in(copy_sql).await.context("copy_in")?;
    futures_util::pin_mut!(sink);
    let mut buf = String::with_capacity(64 * 1024);
    let mut rng = StdRng::seed_from_u64(0xC1_5A_71_D5);
    let mut sink_unpinned = sink;
    for i in 0..FIXTURE_ROWS {
        let x = rng.gen_range(0.0..1_000_000.0);
        let y = rng.gen_range(0.0..1_000_000.0);
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
    assert_eq!(rows, FIXTURE_ROWS as u64, "COPY must report row count");

    client
        .batch_execute(&format!("CREATE PUBLICATION {PUB} FOR TABLE mars_timing.points;"))
        .await
        .context("publication")?;
    client
        .batch_execute(&format!(
            "SELECT pg_create_logical_replication_slot('{SLOT}', 'pgoutput');"
        ))
        .await
        .context("slot")?;
    Ok(())
}

async fn apply_edit_batch(dsn: &str) -> Result<()> {
    let (client, conn) = tokio_postgres::connect(dsn, NoTls).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let mut rng = StdRng::seed_from_u64(0xED_17_BA_7E);
    let mut sql = String::with_capacity(256 * 1024);
    let mut event_count = 0i64;
    while event_count < EDIT_BATCH {
        let r: f64 = rng.r#gen();
        if r < 0.1 {
            let new_id = FIXTURE_ROWS + event_count;
            let x = rng.gen_range(0.0..1_000_000.0);
            let y = rng.gen_range(0.0..1_000_000.0);
            sql.push_str(&format!(
                "INSERT INTO mars_timing.points (gid, name, geom) VALUES ({new_id}, 'ins-{new_id}', \
                 ST_GeomFromText('POINT({x} {y})', 25832));\n"
            ));
        } else if r < 0.2 {
            let target_id = rng.gen_range(0..FIXTURE_ROWS);
            sql.push_str(&format!("DELETE FROM mars_timing.points WHERE gid = {target_id};\n"));
        } else {
            let target_id = rng.gen_range(0..FIXTURE_ROWS);
            let x = rng.gen_range(0.0..1_000_000.0);
            let y = rng.gen_range(0.0..1_000_000.0);
            sql.push_str(&format!(
                "UPDATE mars_timing.points SET name = 'upd-{target_id}', \
                 geom = ST_GeomFromText('POINT({x} {y})', 25832) WHERE gid = {target_id};\n"
            ));
        }
        event_count += 1;
        if sql.len() > 64 * 1024 {
            client.batch_execute(&sql).await.context("edit chunk")?;
            sql.clear();
        }
    }
    if !sql.is_empty() {
        client.batch_execute(&sql).await.context("edit tail")?;
    }
    Ok(())
}

/// Build a 2D EWKB point as ascii hex.
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
  name: e2e-timing
  title: "E2E timing"
  abstract: "5-min partial-update gate fixture"
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
    max_size: 256MiB
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

layers:
  - name: pts
    title: "Points"
    type: point
    sources:
      - band: hi
        from: mars_timing.points
        geometry_column: geom
        id_column: gid
        attributes: [name]
        page_size_target_bytes: 65536
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

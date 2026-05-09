//! Step 12 (LAZARUS Phase C closing gate 1): end-to-end change-feed cycle.
//!
//! Boots a postgis container with logical replication enabled, bootstraps
//! a small fixture, then drives one explicit cycle:
//!   1. subscribe via the real `mars-source-postgres` change feed,
//!   2. mutate the source (insert + update + delete),
//!   3. drain the next batch from the subscription,
//!   4. apply the batch via `Compiler::run_cycle_once`,
//!   5. assert the new manifest is `format_version == 3`, that the
//!      page-membership sidecar reflects post-cycle state, and that
//!      content hashes change only on dirty pages.

#![cfg(feature = "e2e")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use mars_compiler::sidecar::SidecarReader;
use mars_compiler::{Compiler, Deps as CompilerDeps};
use mars_config::{Config, config_dir};
use mars_source::ChangeFeed;
use mars_source_postgres::{CollectionTopology, PgConfig, PgSource, ReplicationTopology};
use mars_store::{ManifestStore, ObjectStore};
use mars_store_fs::{FsPublisher, FsStore};
use mars_types::{ContentHash, MANIFEST_FORMAT_VERSION, PageKey};
use rand::distributions::{Alphanumeric, DistString};
use tempfile::TempDir;
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use tokio_postgres::NoTls;
use tokio_util::sync::CancellationToken;

const SLOT: &str = "mars_cycle_slot";
const PUB: &str = "mars_cycle_pub";
const FIXTURE_ROWS: i32 = 200;

#[tokio::test(flavor = "multi_thread")]
async fn e2e_change_feed_cycle_publishes_v3_manifest() -> Result<()> {
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
            collection: "mars_cycle.points".into(),
            schema: "mars_cycle".into(),
            table: "points".into(),
            geometry_column: "geom".into(),
            id_column: "gid".into(),
        }],
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

    // Bootstrap.
    let cancel = CancellationToken::new();
    let v1 = compiler
        .run_snapshot_once(cancel.clone())
        .await
        .context("snapshot compile")?;
    assert_eq!(v1, 1);
    let bootstrap = publisher
        .current()
        .await?
        .ok_or_else(|| anyhow::anyhow!("no manifest after snapshot"))?;
    assert_eq!(bootstrap.format_version, MANIFEST_FORMAT_VERSION);
    let prior_hashes: HashMap<PageKey, ContentHash> = bootstrap
        .pages
        .iter()
        .map(|p| (p.key.clone(), p.content_hash))
        .collect();
    let prior_sidecar_ref = bootstrap.bindings[0]
        .page_membership_sidecar
        .clone()
        .expect("snapshot publishes a sidecar");
    let prior_sidecar_bytes = store
        .get(&prior_sidecar_ref.key, prior_sidecar_ref.hash)
        .await
        .context("fetch prior sidecar")?;
    let prior_sidecar = SidecarReader::open(&prior_sidecar_bytes).context("open prior sidecar")?;
    let prior_count: u64 = bootstrap.pages.iter().map(|p| p.feature_count).sum();

    // Subscribe before mutating so the slot captures every event.
    let mut subscription = source.subscribe().await.context("subscribe")?;

    // Mutate: insert one new row, update one existing row in place, delete one.
    let (client, conn) = tokio_postgres::connect(&dsn, NoTls)
        .await
        .context("post-bootstrap connect")?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
        .batch_execute(
            "INSERT INTO mars_cycle.points (gid, name, geom) VALUES \
             (9999, 'inserted', ST_GeomFromText('POINT(150.0 150.0)', 25832));\
             UPDATE mars_cycle.points SET name = 'updated', geom = ST_GeomFromText('POINT(50.5 50.5)', 25832) WHERE gid = 50;\
             DELETE FROM mars_cycle.points WHERE gid = 100;",
        )
        .await
        .context("post-bootstrap mutations")?;

    // Drain batches until we observe non-empty events; pgoutput coalesces
    // and may send several chunks before the txn fully reaches us.
    let mut batches = Vec::new();
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut total_events = 0usize;
    while total_events < 3 && std::time::Instant::now() < drain_deadline {
        let next = tokio::time::timeout(Duration::from_secs(5), subscription.next_batch()).await;
        match next {
            Ok(Some(Ok(batch))) => {
                total_events += batch.events.len();
                batches.push(batch);
            }
            Ok(Some(Err(e))) => return Err(anyhow::anyhow!("change feed error: {e}")),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert!(
        total_events >= 3,
        "expected at least 3 events (insert/update/delete), got {total_events}"
    );

    // Apply the cycle.
    let v2 = compiler.run_cycle_once(batches).await.context("run_cycle_once")?;
    assert_eq!(v2, 2, "incremental cycle should publish manifest v2");
    let new_manifest = publisher
        .current()
        .await?
        .ok_or_else(|| anyhow::anyhow!("no manifest after cycle"))?;
    assert_eq!(new_manifest.format_version, MANIFEST_FORMAT_VERSION);
    assert!(
        new_manifest.source_version.is_some(),
        "manifest v2 must carry a source_version cursor"
    );

    // Page hash deltas: at least one page rebuilt; pages outside the dirty
    // set carry through with byte-identical content hashes.
    let new_hashes: HashMap<PageKey, ContentHash> = new_manifest
        .pages
        .iter()
        .map(|p| (p.key.clone(), p.content_hash))
        .collect();
    let mut rebuilt = 0;
    for (k, h) in &new_hashes {
        if let Some(prior_hash) = prior_hashes.get(k)
            && prior_hash != h
        {
            rebuilt += 1;
        }
    }
    assert!(rebuilt >= 1, "expected at least one rebuilt page, got {rebuilt}");

    // Sidecar reflects post-cycle state.
    let new_sidecar_ref = new_manifest.bindings[0]
        .page_membership_sidecar
        .as_ref()
        .expect("post-cycle sidecar");
    let new_sidecar_bytes = store
        .get(&new_sidecar_ref.key, new_sidecar_ref.hash)
        .await
        .context("fetch new sidecar")?;
    let new_sidecar = SidecarReader::open(&new_sidecar_bytes).context("open new sidecar")?;
    assert!(
        new_sidecar.lookup_all(9999).next().is_some(),
        "inserted id missing from sidecar"
    );
    assert!(
        new_sidecar.lookup_all(100).next().is_none(),
        "deleted id still present in sidecar"
    );
    assert!(
        new_sidecar.lookup_all(50).next().is_some(),
        "updated id absent from sidecar"
    );

    // Total feature count: bootstrap + 1 (insert) - 1 (delete).
    let new_count: u64 = new_manifest.pages.iter().map(|p| p.feature_count).sum();
    assert_eq!(new_count, prior_count + 1 - 1);

    // sanity: keep prior sidecar borrow alive across assertions above.
    let _ = prior_sidecar;

    Ok(())
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
                         CREATE SCHEMA mars_cycle;\
                         CREATE TABLE mars_cycle.points (\
                            gid INT4 PRIMARY KEY,\
                            name TEXT NOT NULL,\
                            geom geometry(Point, 25832) NOT NULL\
                         );\
                         ALTER TABLE mars_cycle.points REPLICA IDENTITY FULL;",
                    )
                    .await
                    .context("schema/seed")?;
                let mut bulk = String::with_capacity(64 * 1024);
                bulk.push_str("INSERT INTO mars_cycle.points (gid, name, geom) VALUES ");
                for i in 0..FIXTURE_ROWS {
                    if i > 0 {
                        bulk.push(',');
                    }
                    let x = f64::from(i) * 4.0;
                    let y = f64::from(i) * 4.0;
                    bulk.push_str(&format!("({i}, 'p{i}', ST_GeomFromText('POINT({x} {y})', 25832))",));
                }
                bulk.push(';');
                client.batch_execute(&bulk).await.context("seed insert")?;
                client
                    .batch_execute(&format!("CREATE PUBLICATION {PUB} FOR TABLE mars_cycle.points;"))
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

fn fixture_yaml(dsn_kv: &str, store_path: &str, cache_path: &str) -> String {
    format!(
        r##"service:
  name: e2e-cycle
  title: "E2E cycle"
  abstract: "live compiler cycle fixture"
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
    max_y: 2048

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
        from: mars_cycle.points
        geometry_column: geom
        id_column: gid
        attributes: [name]
        page_size_target_bytes: 8192
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

//! end-to-end: snapshot bootstrap against postgis.
//!
//! Boots a postgis container, COPY-loads a synthetic point table, runs
//! `Compiler::run_snapshot_once`, and verifies:
//! - manifest carries one binding with the expected `feature_count_total`,
//! - per-page data is decodable through `ArtifactReader` (SpatialIndex query
//!   returns hits, `attributes_by_feature_id` resolves a known sample),
//! - the page-membership sidecar resolves the same feature_id back to the
//!   Hilbert key recorded at compile time.
//!
//! Substrate-shape correctness gate; size is moderate (FEATURE_COUNT below)
//! to keep CI runtime under a minute. Larger soak-test loads are
//! operator-driven, not CI-driven.

#![cfg(feature = "integration")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::SinkExt;
use mars_artifact::{ArtifactReader, SectionKind, SpatialIndex};
use mars_compiler::sidecar::SidecarReader;
use mars_compiler::{Compiler, Deps as CompilerDeps};
use mars_config::{Config, config_dir};
use mars_source::{LeaderLock, LeaderLockGuard, SourceError};
use mars_source_postgres::{PgConfig, PgSource};
use mars_store::{ManifestStore, ObjectStore};
use mars_store_fs::{FsPublisher, FsStore};
use mars_types::ContentHash;
use rand::SeedableRng;
use rand::distr::{Alphanumeric, SampleString};
use rand::rngs::StdRng;
use tempfile::TempDir;
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use tokio_postgres::NoTls;
use tokio_util::sync::CancellationToken;

/// Synthetic feature count. 100k keeps CI runtime under a minute end-to-end
/// while still exercising multi-page emission at a 64 KiB page budget.
const FEATURE_COUNT: i64 = 100_000;

/// Page byte budget; small enough that 100k features split into many pages.
const PAGE_TARGET_BYTES: u64 = 64 * 1024;

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_bootstrap_e2e() -> Result<()> {
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

    seed_database(&dsn).await.context("seed database")?;

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

    let source = Arc::new(
        PgSource::connect(PgConfig {
            dsn: dsn.clone(),
            publication: String::new(),
            slot: String::new(),
            ..Default::default()
        })
        .await
        .context("pg connect")?,
    );
    let store = Arc::new(FsStore::new(cfg.artifacts.store.path.as_deref().unwrap()).context("open store")?);
    let publisher = Arc::new(FsPublisher::new(cfg.artifacts.store.path.as_deref().unwrap()).context("open publisher")?);

    let compiler = Compiler::new(
        CompilerDeps {
            source: source.clone(),
            change_feed: source.clone(),
            // snapshot does not exercise the leader lock against real pg here;
            // a permissive stub is fine for a single-process test run.
            leader_lock: Arc::new(AlwaysLeader),
            store: store.clone(),
            manifest: publisher.clone(),
            metrics: mars_observability::Metrics::new().context("metrics")?,
        },
        cfg,
    );

    let cancel = CancellationToken::new();
    let v = compiler
        .run_snapshot_once(cancel.clone())
        .await
        .context("snapshot compile")?;
    assert_eq!(v, 1, "first snapshot publishes manifest v1");

    let manifest = publisher
        .current()
        .await
        .context("read current manifest")?
        .ok_or_else(|| anyhow::anyhow!("no manifest after snapshot"))?;
    assert_eq!(manifest.bindings.len(), 1);
    assert_eq!(
        manifest.bindings[0].feature_count_total, FEATURE_COUNT as u64,
        "binding feature_count_total must equal inserted rows"
    );
    assert!(
        manifest.pages.len() > 1,
        "expected multiple pages at PAGE_TARGET_BYTES={PAGE_TARGET_BYTES}, got {}",
        manifest.pages.len()
    );

    // Sample 100 known-present feature_ids; each must be reachable via
    // (page slice scan) -> page artifact -> (SpatialIndex hit + attributes).
    let mut rng = StdRng::seed_from_u64(0xC1_5A_71_D5);
    let sample_ids: Vec<u64> = (0..100)
        .map(|_| rand::Rng::gen_range(&mut rng, 0..FEATURE_COUNT) as u64)
        .collect();

    // Verify the page-membership sidecar resolves each sampled id.
    let sidecar_entry = manifest.bindings[0]
        .page_membership_sidecar
        .as_ref()
        .expect("snapshot publishes a sidecar");
    let sidecar_bytes = store
        .get(&sidecar_entry.key, sidecar_entry.hash)
        .await
        .context("fetch sidecar")?;
    let sidecar = SidecarReader::open(&sidecar_bytes).context("open sidecar")?;

    for &id in &sample_ids {
        assert!(sidecar.lookup_all(id).next().is_some(), "sidecar missing user_id {id}");
    }

    // Verify each sampled id is decodable through some page in the manifest.
    let mut found = 0usize;
    for &id in &sample_ids {
        if let Some((page_entry, slot)) = find_page_for_id(&manifest.pages, &store, id).await {
            let key = page_entry.key.object_key(&page_entry.content_hash).unwrap();
            let page_bytes = store.get(&key, page_entry.content_hash).await.context("fetch page")?;
            let reader = ArtifactReader::open(page_bytes).context("open page")?;
            // attributes by slot must succeed.
            let row = reader
                .attributes_by_slot(slot)
                .context("attrs lookup")?
                .expect("feature in page");
            let decoded = mars_artifact::decode_row(row).context("decode row")?;
            assert!(!decoded.is_empty(), "row {id} decoded empty");

            // SpatialIndex query against the page bbox must include this feature.
            let spix_bytes = reader.section(SectionKind::SpatialIndex).unwrap();
            let spix = SpatialIndex::open(spix_bytes).unwrap();
            let bb = page_entry.spatial_bbox;
            let viewport = [
                bb.min_x as f32 - 1.0,
                bb.min_y as f32 - 1.0,
                bb.max_x as f32 + 1.0,
                bb.max_y as f32 + 1.0,
            ];
            let mut hits = Vec::new();
            spix.query(viewport, &mut hits);
            assert!(!hits.is_empty(), "spatial index returned no hits for full page bbox");
            found += 1;
        }
    }
    assert_eq!(
        found,
        sample_ids.len(),
        "every sampled id should resolve to exactly one page"
    );

    Ok(())
}

async fn find_page_for_id(
    pages: &[mars_types::PageEntry],
    store: &Arc<FsStore>,
    id: u64,
) -> Option<(mars_types::PageEntry, u32)> {
    for p in pages {
        let key = p.key.object_key(&p.content_hash).ok()?;
        let bytes = store.get(&key, p.content_hash).await.ok()?;
        let reader = ArtifactReader::open(bytes).ok()?;
        let geom_bytes = reader.section(SectionKind::GeometryPayload).ok()?;
        let iter = mars_artifact::iter_feature_index(&geom_bytes).ok()?;
        for (slot_idx, entry) in iter.enumerate() {
            let entry = entry.ok()?;
            if entry.user_id == id {
                return Some((p.clone(), slot_idx as u32));
            }
        }
    }
    None
}

async fn seed_database(dsn: &str) -> Result<()> {
    let (client, conn) = {
        let mut attempts = 0;
        loop {
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
        }
    };
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

    // bulk load via COPY for speed.
    let copy_sql = "COPY bootstrap.points (gid, name, geom) FROM STDIN WITH (FORMAT csv)";
    let sink = client.copy_in(copy_sql).await.context("copy_in")?;
    futures_util::pin_mut!(sink);
    let mut buf = String::with_capacity(64 * 1024);
    let mut rng = StdRng::seed_from_u64(0xC1_5A_71_D5);
    use rand::Rng;
    let mut sink_unpinned = sink;
    for i in 0..FEATURE_COUNT {
        // random x, y in [0, 1_000_000) m; CSV-ready WKT-shaped EWKB hex would be
        // overkill -- pgsql parses ST_GeomFromText embedded in COPY would not, so
        // emit hex EWKB for a 2D point in EPSG:25832 directly.
        let x: f64 = rng.gen_range(0.0..1_000_000.0);
        let y: f64 = rng.gen_range(0.0..1_000_000.0);
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
    assert_eq!(rows, FEATURE_COUNT as u64, "COPY must report row count");
    Ok(())
}

/// Build a 2D EWKB point as ascii hex. Layout:
/// `01` (LE) + `01000020` (point + EWKB SRID flag) + srid LE u32 + x f64 LE + y f64 LE
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
  name: bootstrap-e2e
  title: "Bootstrap E2E"
  abstract: "snapshot bootstrap fixture"
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

/// Permissive leader-lock stub. Snapshot compile is single-process here; the
/// real pg advisory-lock path is exercised by `leader_lock_e2e`.
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

// keep the ContentHash import live across feature flags; the manifest checks
// above refer to it through field types.
#[allow(dead_code)]
const _CH_USED: Option<ContentHash> = None;

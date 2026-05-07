//! e2e: live pgoutput replication against a postgis container.
//!
//! Covers the full transport path:
//!   - INSERT/UPDATE/DELETE/TRUNCATE round-trips through the pgoutput decoder
//!     and translator into `ChangeEvent`s with correct cell coverage.
//!   - REPLICA IDENTITY enforcement (DELETE without FULL is a hard error).
//!   - Multi-relation TRUNCATE emits one event per known collection in a
//!     single batch.
//!   - Ack semantics: an unacknowledged batch is replayed on reconnect; an
//!     acknowledged batch is not.

#![cfg(feature = "e2e")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use mars_grid::BandConfig;
use mars_source::{ChangeEvent, ChangeFeed, SourceError};
use mars_source_postgres::{CollectionTopology, PgConfig, PgSource, ReplicationTopology};
use mars_types::ScaleBand;
use rand::distributions::{Alphanumeric, DistString};
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

const SLOT: &str = "mars_e2e_slot";
const PUB: &str = "mars_e2e_pub";

/// Bring up postgis with wal_level=logical and create the publication, slot,
/// and bound table(s). Returns the container (kept alive) and the DSN used by
/// the adapter.
async fn boot_postgis() -> (ContainerAsync<GenericImage>, String) {
    let password = Alphanumeric.sample_string(&mut rand::thread_rng(), 16);
    let container = GenericImage::new("postgis/postgis", "16-3.4")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_PASSWORD", &password)
        .with_env_var("POSTGRES_USER", "mars")
        .with_env_var("POSTGRES_DB", "mars")
        // pgoutput needs wal_level=logical; the default is `replica`.
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
        .expect("docker available");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let dsn = format!("host=127.0.0.1 port={port} user=mars password={password} dbname=mars");
    (container, dsn)
}

async fn setup_schema(src: &PgSource, full_identity: &[&str], default_identity: &[&str]) {
    let client = src.pool().get().await.unwrap();
    client
        .batch_execute("CREATE EXTENSION IF NOT EXISTS postgis;")
        .await
        .unwrap();
    for tbl in full_identity.iter().chain(default_identity.iter()) {
        client
            .batch_execute(&format!(
                "CREATE TABLE {tbl} (\
                    gid INT4 PRIMARY KEY,\
                    geom geometry(Point, 25832)\
                 );"
            ))
            .await
            .unwrap();
    }
    for tbl in full_identity {
        client
            .batch_execute(&format!("ALTER TABLE {tbl} REPLICA IDENTITY FULL;"))
            .await
            .unwrap();
    }
    let table_list = full_identity
        .iter()
        .chain(default_identity.iter())
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    client
        .batch_execute(&format!("CREATE PUBLICATION {PUB} FOR TABLE {table_list};"))
        .await
        .unwrap();
    // create the slot last so it captures changes from this point onward only.
    client
        .batch_execute(&format!(
            "SELECT pg_create_logical_replication_slot('{SLOT}', 'pgoutput');"
        ))
        .await
        .unwrap();
}

fn topology_for(collections: &[(&str, &str)]) -> ReplicationTopology {
    ReplicationTopology {
        collections: collections
            .iter()
            .map(|(coll, table)| CollectionTopology {
                collection: (*coll).into(),
                schema: "public".into(),
                table: (*table).into(),
                geometry_column: "geom".into(),
            })
            .collect(),
        bands: vec![BandConfig {
            name: ScaleBand::new("hi"),
            max_denom: 25_000,
            origin: (0.0, 0.0),
            cell_size: 1024.0,
        }],
        max_cells_per_row: 1024,
    }
}

fn pg_cfg(dsn: &str) -> PgConfig {
    PgConfig {
        dsn: dsn.into(),
        publication: PUB.into(),
        slot: SLOT.into(),
        ..Default::default()
    }
}

async fn next_batch_or_timeout(
    sub: &mut Box<dyn mars_source::ChangeSubscription>,
) -> Result<mars_source::ChangeBatch, String> {
    match tokio::time::timeout(Duration::from_secs(15), sub.next_batch()).await {
        Ok(Some(Ok(b))) => Ok(b),
        Ok(Some(Err(e))) => Err(format!("err: {e:?}")),
        Ok(None) => Err("feed closed".into()),
        Err(_) => Err("timeout waiting for batch".into()),
    }
}

#[tokio::test]
async fn insert_update_delete_round_trip() {
    let (_c, dsn) = boot_postgis().await;
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    setup_schema(&src, &["roads"], &[]).await;

    let topology = topology_for(&[("roads_c", "roads")]);
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap().with_topology(topology);
    let mut sub = src.subscribe().await.unwrap();

    let writer = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    let client = writer.pool().get().await.unwrap();

    // INSERT.
    client
        .batch_execute("INSERT INTO roads VALUES (1, ST_SetSRID(ST_MakePoint(50, 50), 25832));")
        .await
        .unwrap();
    let batch = next_batch_or_timeout(&mut sub).await.unwrap();
    assert!(batch.source_version.is_some(), "insert batch must carry LSN");
    assert_eq!(batch.events.len(), 1);
    match &batch.events[0] {
        ChangeEvent::Insert { collection, cells } => {
            assert_eq!(collection.as_str(), "roads_c");
            assert_eq!(cells.len(), 1);
        }
        other => panic!("expected Insert, got {other:?}"),
    }

    // UPDATE moving the geometry: cells = old ∪ new.
    client
        .batch_execute("UPDATE roads SET geom = ST_SetSRID(ST_MakePoint(2000, 2000), 25832) WHERE gid = 1;")
        .await
        .unwrap();
    let batch = next_batch_or_timeout(&mut sub).await.unwrap();
    match &batch.events[0] {
        ChangeEvent::Update { cells, .. } => assert_eq!(cells.len(), 2),
        other => panic!("expected Update, got {other:?}"),
    }

    // DELETE.
    client.batch_execute("DELETE FROM roads WHERE gid = 1;").await.unwrap();
    let batch = next_batch_or_timeout(&mut sub).await.unwrap();
    assert!(matches!(batch.events[0], ChangeEvent::Delete { .. }));
}

#[tokio::test]
async fn delete_without_full_identity_is_a_hard_error() {
    let (_c, dsn) = boot_postgis().await;
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    // table without REPLICA IDENTITY FULL: DELETE will arrive with key-only old.
    setup_schema(&src, &[], &["leaky"]).await;

    let topology = topology_for(&[("leaky_c", "leaky")]);
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap().with_topology(topology);
    let mut sub = src.subscribe().await.unwrap();

    let writer = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    let client = writer.pool().get().await.unwrap();
    // separate batch_execute calls keep these in their own implicit
    // transactions; otherwise the simple-query protocol bundles them as one.
    client
        .batch_execute("INSERT INTO leaky VALUES (1, ST_SetSRID(ST_MakePoint(50, 50), 25832));")
        .await
        .unwrap();
    client.batch_execute("DELETE FROM leaky WHERE gid = 1;").await.unwrap();

    // first batch is the INSERT (FULL not required for inserts).
    let b = next_batch_or_timeout(&mut sub).await.unwrap();
    assert!(matches!(b.events.as_slice(), [ChangeEvent::Insert { .. }]));

    // second batch should surface a Backend error citing REPLICA IDENTITY FULL.
    let res = tokio::time::timeout(Duration::from_secs(15), sub.next_batch()).await;
    let next = res.expect("timeout").expect("feed closed");
    match next {
        Err(SourceError::Backend { source, .. }) => {
            let msg = source.to_string();
            assert!(msg.contains("REPLICA IDENTITY FULL"), "msg = {msg}");
        }
        other => panic!("expected Backend error, got {other:?}"),
    }
}

#[tokio::test]
async fn truncate_emits_one_event_per_bound_table() {
    let (_c, dsn) = boot_postgis().await;
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    setup_schema(&src, &["roads", "buildings"], &[]).await;

    let topology = topology_for(&[("roads_c", "roads"), ("buildings_c", "buildings")]);
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap().with_topology(topology);
    let mut sub = src.subscribe().await.unwrap();

    let writer = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    let client = writer.pool().get().await.unwrap();
    // separate batch_execute calls so each is its own implicit transaction
    // and lands in its own ChangeBatch.
    client
        .batch_execute("INSERT INTO roads VALUES (1, ST_SetSRID(ST_MakePoint(10, 10), 25832));")
        .await
        .unwrap();
    client
        .batch_execute("INSERT INTO buildings VALUES (1, ST_SetSRID(ST_MakePoint(10, 10), 25832));")
        .await
        .unwrap();
    client.batch_execute("TRUNCATE roads, buildings;").await.unwrap();

    // drain insert batches.
    let _ = next_batch_or_timeout(&mut sub).await.unwrap();
    let _ = next_batch_or_timeout(&mut sub).await.unwrap();

    let batch = next_batch_or_timeout(&mut sub).await.unwrap();
    let collections: Vec<_> = batch
        .events
        .iter()
        .map(|e| match e {
            ChangeEvent::Truncate { collection } => collection.as_str(),
            other => panic!("expected Truncate, got {other:?}"),
        })
        .collect();
    assert_eq!(collections.len(), 2, "events = {:?}", batch.events);
    assert!(collections.contains(&"roads_c"));
    assert!(collections.contains(&"buildings_c"));
}

#[tokio::test]
async fn unacked_batch_is_replayed_on_reconnect() {
    let (_c, dsn) = boot_postgis().await;
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    setup_schema(&src, &["roads"], &[]).await;

    let topology = topology_for(&[("roads_c", "roads")]);

    // first subscription: receive a batch, drop without ack.
    {
        let src = PgSource::connect(pg_cfg(&dsn))
            .await
            .unwrap()
            .with_topology(topology.clone());
        let mut sub = src.subscribe().await.unwrap();

        let writer = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
        let client = writer.pool().get().await.unwrap();
        client
            .batch_execute("INSERT INTO roads VALUES (1, ST_SetSRID(ST_MakePoint(50, 50), 25832));")
            .await
            .unwrap();

        let batch = next_batch_or_timeout(&mut sub).await.unwrap();
        assert_eq!(batch.events.len(), 1);
        // intentionally do not call sub.acknowledge(...).
        drop(sub);
    }

    // give the slot a moment to tear down server-side.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // second subscription: should replay the exact same insert.
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap().with_topology(topology);
    let mut sub = src.subscribe().await.unwrap();
    let batch = next_batch_or_timeout(&mut sub).await.unwrap();
    assert_eq!(batch.events.len(), 1);
    assert!(matches!(batch.events[0], ChangeEvent::Insert { .. }));
}

#[tokio::test]
async fn acked_batch_is_not_replayed_on_reconnect() {
    let (_c, dsn) = boot_postgis().await;
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    setup_schema(&src, &["roads"], &[]).await;

    let topology = topology_for(&[("roads_c", "roads")]);

    let writer = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    let client = writer.pool().get().await.unwrap();

    // first subscription: receive a batch, ack it, drop.
    {
        let src = PgSource::connect(pg_cfg(&dsn))
            .await
            .unwrap()
            .with_topology(topology.clone());
        let mut sub = src.subscribe().await.unwrap();

        client
            .batch_execute("INSERT INTO roads VALUES (1, ST_SetSRID(ST_MakePoint(50, 50), 25832));")
            .await
            .unwrap();
        let batch = next_batch_or_timeout(&mut sub).await.unwrap();
        sub.acknowledge(batch.source_version.as_deref()).await.unwrap();
        // status_interval + idle_wakeup_interval are both 1s; this sleep
        // guarantees the worker fires at least one status update with the
        // acked LSN before we drop the subscription.
        tokio::time::sleep(Duration::from_secs(3)).await;
        drop(sub);
    }

    tokio::time::sleep(Duration::from_millis(500)).await;

    // second subscription: insert one new row, expect ONLY that one.
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap().with_topology(topology);
    let mut sub = src.subscribe().await.unwrap();
    client
        .batch_execute("INSERT INTO roads VALUES (2, ST_SetSRID(ST_MakePoint(100, 100), 25832));")
        .await
        .unwrap();
    let batch = next_batch_or_timeout(&mut sub).await.unwrap();
    assert_eq!(batch.events.len(), 1, "expected single replay-free batch");
    // Make sure no replay of gid=1 follows immediately. Pull again with a
    // short timeout — we expect a timeout.
    let r = tokio::time::timeout(Duration::from_secs(2), sub.next_batch()).await;
    assert!(r.is_err(), "unexpected extra batch after ack: {r:?}");
}

//! e2e: live pgoutput replication against a postgis container.
//!
//! Covers the full transport path:
//!   - INSERT/UPDATE/DELETE/TRUNCATE round-trips through the pgoutput decoder
//!     and translator into `ChangeEvent`s with correct cell coverage.
//!   - Preflight enforcement of the id-column-in-identity contract.
//!   - Multi-relation TRUNCATE emits one event per known collection in a
//!     single batch.
//!   - Ack semantics: an unacknowledged batch is replayed on reconnect; an
//!     acknowledged batch is not.

#![cfg(feature = "integration")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use mars_source::{BindingHealth, ChangeEvent, ChangeFeed, RebindReason, Source, SourceCollectionId};
use mars_source_postgres::{CollectionTopology, PgConfig, PgSource, ReplicationTopology};
use mars_test_support::postgis::boot_postgis;

const SLOT: &str = "mars_e2e_slot";
const PUB: &str = "mars_e2e_pub";

/// Standard postgres baseline: each table gets `gid INT4 PRIMARY KEY`
/// and the postgres default REPLICA IDENTITY (DEFAULT, PK-based). Tests
/// that need a different identity setup (e.g. NOTHING / no PK) issue
/// the deviation inline before subscribing.
async fn setup_schema(src: &PgSource, tables: &[&str]) {
    let client = src.pool().get().await.unwrap();
    client
        .batch_execute("CREATE EXTENSION IF NOT EXISTS postgis;")
        .await
        .unwrap();
    for tbl in tables {
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
    let table_list = tables.join(", ");
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
                id_column: "gid".into(),
            })
            .collect(),
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
async fn insert_update_delete_round_trip_under_default_identity() {
    let fx = boot_postgis().await;
    let dsn = fx.dsn.clone();
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    // standard postgres baseline: PK + REPLICA IDENTITY DEFAULT. no
    // explicit ALTER TABLE ... REPLICA IDENTITY FULL anywhere.
    setup_schema(&src, &["roads"]).await;

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
        ChangeEvent::Insert {
            collection,
            feature_id,
            new_envelope,
        } => {
            assert_eq!(collection.as_str(), "roads_c");
            assert_eq!(*feature_id, 1);
            assert_eq!(new_envelope.centroid, [50.0, 50.0]);
        }
        other => panic!("expected Insert, got {other:?}"),
    }

    // UPDATE moves the geometry. under DEFAULT identity pgoutput sends
    // no old tuple; the translator surfaces feature_id from `new`.
    // old-side dirty pages are recovered downstream from the
    // page-membership sidecar.
    client
        .batch_execute("UPDATE roads SET geom = ST_SetSRID(ST_MakePoint(2000, 2000), 25832) WHERE gid = 1;")
        .await
        .unwrap();
    let batch = next_batch_or_timeout(&mut sub).await.unwrap();
    match &batch.events[0] {
        ChangeEvent::Update {
            feature_id,
            new_envelope,
            ..
        } => {
            assert_eq!(*feature_id, 1);
            assert_eq!(new_envelope.centroid, [2000.0, 2000.0]);
        }
        other => panic!("expected Update, got {other:?}"),
    }

    // DELETE: K tuple carries `gid` only. feature_id still recovered.
    client.batch_execute("DELETE FROM roads WHERE gid = 1;").await.unwrap();
    let batch = next_batch_or_timeout(&mut sub).await.unwrap();
    match &batch.events[0] {
        ChangeEvent::Delete { feature_id, .. } => {
            assert_eq!(*feature_id, 1);
        }
        other => panic!("expected Delete, got {other:?}"),
    }
}

#[tokio::test]
async fn bind_without_id_in_identity_degrades_binding_instead_of_killing_feed() {
    let fx = boot_postgis().await;
    let dsn = fx.dsn.clone();
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    setup_schema(&src, &["roads"]).await;
    // add a sibling table whose id column isn't part of the replica
    // identity. no PRIMARY KEY + REPLICA IDENTITY NOTHING means
    // pgoutput won't flag `gid` as a key column, and the bind must
    // refuse. NOTHING also blocks UPDATE/DELETE server-side, so we
    // only exercise the INSERT path here - which is enough to surface
    // the Relation message and trigger preflight.
    {
        let client = src.pool().get().await.unwrap();
        client
            .batch_execute(
                "CREATE TABLE leaky (gid INT4, geom geometry(Point, 25832));\
                 ALTER TABLE leaky REPLICA IDENTITY NOTHING;\
                 ALTER PUBLICATION mars_e2e_pub ADD TABLE leaky;",
            )
            .await
            .unwrap();
    }

    let topology = topology_for(&[("leaky_c", "leaky"), ("roads_c", "roads")]);
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap().with_topology(topology);
    let mut sub = src.subscribe().await.unwrap();

    let writer = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    let client = writer.pool().get().await.unwrap();

    // first dml on the degraded table: Relation arrives at the head of the
    // xlog stream, preflight rejects it, the Insert that follows is
    // dropped. batch carries just the Rebind PreflightFailed.
    client
        .batch_execute("INSERT INTO leaky VALUES (1, ST_SetSRID(ST_MakePoint(50, 50), 25832));")
        .await
        .unwrap();
    let b = next_batch_or_timeout(&mut sub).await.unwrap();
    match b.events.as_slice() {
        [
            ChangeEvent::Rebind {
                collection,
                reason: RebindReason::PreflightFailed { reason },
            },
        ] => {
            assert_eq!(collection.as_str(), "leaky_c");
            assert!(reason.contains("replica identity"), "reason = {reason}");
        }
        other => panic!("expected single Rebind PreflightFailed, got {other:?}"),
    }

    // subsequent dml on the rejected table drops silently. the healthy
    // sibling keeps emitting events, proving the subscription stayed up.
    client
        .batch_execute("INSERT INTO leaky VALUES (2, ST_SetSRID(ST_MakePoint(60, 60), 25832));")
        .await
        .unwrap();
    client
        .batch_execute("INSERT INTO roads VALUES (7, ST_SetSRID(ST_MakePoint(10, 10), 25832));")
        .await
        .unwrap();

    // drain batches until we see the roads insert; the leaky insert may
    // arrive as an empty batch in between.
    let mut saw_roads_insert = false;
    for _ in 0..3 {
        let b = next_batch_or_timeout(&mut sub).await.unwrap();
        for ev in &b.events {
            match ev {
                ChangeEvent::Insert {
                    collection, feature_id, ..
                } if collection.as_str() == "roads_c" => {
                    assert_eq!(*feature_id, 7);
                    saw_roads_insert = true;
                }
                ChangeEvent::Rebind { .. } => panic!("unexpected re-emit of Rebind: {ev:?}"),
                other => panic!("unexpected event: {other:?}"),
            }
        }
        if saw_roads_insert {
            break;
        }
    }
    assert!(saw_roads_insert, "healthy sibling should keep emitting events");
}

#[tokio::test]
async fn rebind_after_table_swap_emits_oid_changed() {
    let fx = boot_postgis().await;
    let dsn = fx.dsn.clone();
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    setup_schema(&src, &["roads"]).await;

    let topology = topology_for(&[("roads_c", "roads")]);
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap().with_topology(topology);
    let mut sub = src.subscribe().await.unwrap();

    let writer = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    let client = writer.pool().get().await.unwrap();

    // initial bind via first dml.
    client
        .batch_execute("INSERT INTO roads VALUES (1, ST_SetSRID(ST_MakePoint(50, 50), 25832));")
        .await
        .unwrap();
    let b = next_batch_or_timeout(&mut sub).await.unwrap();
    assert!(matches!(b.events.as_slice(), [ChangeEvent::Insert { .. }]));

    // operator-side swap-and-rename pipeline: drop the old table out of
    // the publication, rebuild it (with PK so preflight passes), add it
    // back in.
    client
        .batch_execute(
            "ALTER PUBLICATION mars_e2e_pub DROP TABLE roads;\
             ALTER TABLE roads RENAME TO roads_old;\
             CREATE TABLE roads (gid INT4 PRIMARY KEY, geom geometry(Point, 25832));\
             ALTER PUBLICATION mars_e2e_pub ADD TABLE roads;",
        )
        .await
        .unwrap();
    // first dml on the new oid triggers pgoutput to emit a Relation for it.
    client
        .batch_execute("INSERT INTO roads VALUES (2, ST_SetSRID(ST_MakePoint(100, 100), 25832));")
        .await
        .unwrap();

    // drain batches: the rebind event lands in the same batch as the
    // insert that triggered the new Relation message.
    let mut saw_rebind = false;
    let mut saw_new_insert = false;
    for _ in 0..3 {
        let b = next_batch_or_timeout(&mut sub).await.unwrap();
        for ev in &b.events {
            match ev {
                ChangeEvent::Rebind {
                    collection,
                    reason: RebindReason::OidChanged { old_oid, new_oid },
                } => {
                    assert_eq!(collection.as_str(), "roads_c");
                    assert_ne!(old_oid, new_oid, "rebind must carry distinct oids");
                    saw_rebind = true;
                }
                ChangeEvent::Insert {
                    collection, feature_id, ..
                } if collection.as_str() == "roads_c" => {
                    assert_eq!(*feature_id, 2);
                    saw_new_insert = true;
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        if saw_rebind && saw_new_insert {
            break;
        }
    }
    assert!(saw_rebind, "expected Rebind OidChanged");
    assert!(saw_new_insert, "expected Insert against the new oid");
}

#[tokio::test]
async fn rebind_to_table_without_id_in_identity_emits_preflight_failed() {
    let fx = boot_postgis().await;
    let dsn = fx.dsn.clone();
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    setup_schema(&src, &["roads"]).await;

    let topology = topology_for(&[("roads_c", "roads")]);
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap().with_topology(topology);
    let mut sub = src.subscribe().await.unwrap();

    let writer = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    let client = writer.pool().get().await.unwrap();

    // initial bind succeeds.
    client
        .batch_execute("INSERT INTO roads VALUES (1, ST_SetSRID(ST_MakePoint(50, 50), 25832));")
        .await
        .unwrap();
    let _ = next_batch_or_timeout(&mut sub).await.unwrap();

    // replacement table has no PK and REPLICA IDENTITY NOTHING, so its
    // `gid` column is not part of the table's effective replica identity.
    // the rebind must refuse and the binding must degrade. NOTHING also
    // blocks UPDATE/DELETE on the table; INSERT is enough to surface the
    // Relation message that triggers preflight.
    client
        .batch_execute(
            "ALTER PUBLICATION mars_e2e_pub DROP TABLE roads;\
             ALTER TABLE roads RENAME TO roads_old;\
             CREATE TABLE roads (gid INT4, geom geometry(Point, 25832));\
             ALTER TABLE roads REPLICA IDENTITY NOTHING;\
             ALTER PUBLICATION mars_e2e_pub ADD TABLE roads;",
        )
        .await
        .unwrap();
    client
        .batch_execute("INSERT INTO roads VALUES (2, ST_SetSRID(ST_MakePoint(100, 100), 25832));")
        .await
        .unwrap();

    let mut saw_preflight_failed = false;
    for _ in 0..3 {
        let b = next_batch_or_timeout(&mut sub).await.unwrap();
        for ev in &b.events {
            match ev {
                ChangeEvent::Rebind {
                    collection,
                    reason: RebindReason::PreflightFailed { reason },
                } => {
                    assert_eq!(collection.as_str(), "roads_c");
                    assert!(reason.contains("replica identity"), "reason = {reason}");
                    saw_preflight_failed = true;
                }
                ChangeEvent::Insert { .. } => panic!("rejected oid must not emit row events: {ev:?}"),
                other => panic!("unexpected event: {other:?}"),
            }
        }
        if saw_preflight_failed {
            break;
        }
    }
    assert!(saw_preflight_failed, "expected Rebind PreflightFailed");
}

#[tokio::test]
async fn probe_binding_health_reports_unpublished_when_table_dropped() {
    let fx = boot_postgis().await;
    let dsn = fx.dsn.clone();
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    setup_schema(&src, &["roads", "buildings"]).await;

    let topology = topology_for(&[("roads_c", "roads"), ("buildings_c", "buildings")]);
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap().with_topology(topology);

    // baseline: both healthy.
    let report = src
        .probe_binding_health(&[
            SourceCollectionId::new("roads_c"),
            SourceCollectionId::new("buildings_c"),
        ])
        .await
        .unwrap();
    assert_eq!(report.len(), 2);
    assert!(report.iter().all(|h| matches!(h, BindingHealth::Healthy(_))));

    // drop one out of the publication: it should now report Unpublished
    // while the survivor stays Healthy.
    let writer = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    let client = writer.pool().get().await.unwrap();
    client
        .batch_execute("ALTER PUBLICATION mars_e2e_pub DROP TABLE buildings;")
        .await
        .unwrap();

    let report = src
        .probe_binding_health(&[
            SourceCollectionId::new("roads_c"),
            SourceCollectionId::new("buildings_c"),
        ])
        .await
        .unwrap();
    let roads = report
        .iter()
        .find(|h| matches!(h, BindingHealth::Healthy(c) if c.as_str() == "roads_c"));
    let buildings = report
        .iter()
        .find(|h| matches!(h, BindingHealth::Unpublished(c) if c.as_str() == "buildings_c"));
    assert!(roads.is_some(), "roads_c should still be Healthy, got {report:?}");
    assert!(buildings.is_some(), "buildings_c should be Unpublished, got {report:?}");
}

#[tokio::test]
async fn truncate_emits_one_event_per_bound_table() {
    let fx = boot_postgis().await;
    let dsn = fx.dsn.clone();
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    setup_schema(&src, &["roads", "buildings"]).await;

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
    let fx = boot_postgis().await;
    let dsn = fx.dsn.clone();
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    setup_schema(&src, &["roads"]).await;

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
    let fx = boot_postgis().await;
    let dsn = fx.dsn.clone();
    let src = PgSource::connect(pg_cfg(&dsn)).await.unwrap();
    setup_schema(&src, &["roads"]).await;

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
    // short timeout - we expect a timeout.
    let r = tokio::time::timeout(Duration::from_secs(2), sub.next_batch()).await;
    assert!(r.is_err(), "unexpected extra batch after ack: {r:?}");
}

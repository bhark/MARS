//! geometry-type breadth for the replication path. the existing
//! `replication_integration.rs` covers Point/INT4 only; this file pins
//! the envelope round-trip for LineString, Polygon, MultiPolygon,
//! GeometryCollection, EMPTY, NULL, large WKB, and non-default SRID.

#![cfg(feature = "integration")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use mars_source::{ChangeEvent, ChangeFeed};
use mars_source_postgres::{CollectionTopology, PgConfig, PgSource, ReplicationTopology};
use mars_test_support::postgis::{PostgisOptions, boot_postgis_with};

const SLOT: &str = "mars_geom_slot";
const PUB: &str = "mars_geom_pub";

async fn boot() -> (mars_test_support::postgis::PostgisFixture, PgSource) {
    let fix = boot_postgis_with(PostgisOptions::default()).await;
    let src = PgSource::connect(pg_cfg(&fix.dsn)).await.expect("connect to postgis");
    (fix, src)
}

fn pg_cfg(dsn: &str) -> PgConfig {
    PgConfig {
        dsn: dsn.into(),
        publication: PUB.into(),
        slot: SLOT.into(),
        ..Default::default()
    }
}

fn topology(collection: &str, table: &str) -> ReplicationTopology {
    ReplicationTopology {
        collections: vec![CollectionTopology {
            collection: collection.into(),
            schema: "public".into(),
            table: table.into(),
            geometry_column: "geom".into(),
            id_column: "gid".into(),
        }],
    }
}

async fn create_table(src: &PgSource, table: &str, geom_type: &str, srid: i32) {
    let client = src.pool().get().await.unwrap();
    client
        .batch_execute("CREATE EXTENSION IF NOT EXISTS postgis;")
        .await
        .unwrap();
    client
        .batch_execute(&format!(
            "CREATE TABLE {table} (gid INT4 PRIMARY KEY, geom geometry({geom_type}, {srid}));"
        ))
        .await
        .unwrap();
    client
        .batch_execute(&format!("CREATE PUBLICATION {PUB} FOR TABLE {table};"))
        .await
        .unwrap();
    client
        .batch_execute(&format!(
            "SELECT pg_create_logical_replication_slot('{SLOT}', 'pgoutput');"
        ))
        .await
        .unwrap();
}

async fn next_envelope(sub: &mut Box<dyn mars_source::ChangeSubscription>) -> mars_source::GeometryEnvelope {
    let batch = tokio::time::timeout(Duration::from_secs(15), sub.next_batch())
        .await
        .expect("timeout waiting for batch")
        .expect("subscription closed")
        .expect("batch error");
    match batch.events.into_iter().next().expect("at least one event") {
        ChangeEvent::Insert { new_envelope, .. } => new_envelope,
        other => panic!("expected Insert, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn replication_emits_linestring_envelope_with_correct_bbox() {
    let (_fix, src) = boot().await;
    create_table(&src, "lines", "LineString", 25832).await;
    let src = PgSource::connect(pg_cfg(&_fix.dsn))
        .await
        .unwrap()
        .with_topology(topology("lines_c", "lines"));
    let mut sub = src.subscribe().await.unwrap();

    let writer = PgSource::connect(pg_cfg(&_fix.dsn)).await.unwrap();
    let c = writer.pool().get().await.unwrap();
    c.batch_execute(
        "INSERT INTO lines VALUES (1, ST_SetSRID(ST_MakeLine(ST_MakePoint(0,0), ST_MakePoint(100,100)), 25832));",
    )
    .await
    .unwrap();

    let env = next_envelope(&mut sub).await;
    // centroid is roughly the midpoint.
    assert!((env.centroid[0] - 50.0).abs() < 1.0, "centroid x = {}", env.centroid[0]);
    assert!((env.centroid[1] - 50.0).abs() < 1.0, "centroid y = {}", env.centroid[1]);
    // bbox spans the line.
    assert!(
        env.bbox.min_x <= 0.0 && env.bbox.max_x >= 100.0,
        "bbox x: {:?}",
        env.bbox
    );
    assert!(
        env.bbox.min_y <= 0.0 && env.bbox.max_y >= 100.0,
        "bbox y: {:?}",
        env.bbox
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn replication_emits_polygon_envelope_with_centroid_inside_ring() {
    let (_fix, src) = boot().await;
    create_table(&src, "polys", "Polygon", 25832).await;
    let src = PgSource::connect(pg_cfg(&_fix.dsn))
        .await
        .unwrap()
        .with_topology(topology("polys_c", "polys"));
    let mut sub = src.subscribe().await.unwrap();

    let writer = PgSource::connect(pg_cfg(&_fix.dsn)).await.unwrap();
    let c = writer.pool().get().await.unwrap();
    // 100x100 square at origin.
    c.batch_execute(
        "INSERT INTO polys VALUES (1, ST_GeomFromText('POLYGON((0 0, 100 0, 100 100, 0 100, 0 0))', 25832));",
    )
    .await
    .unwrap();

    let env = next_envelope(&mut sub).await;
    assert!((env.centroid[0] - 50.0).abs() < 1.0, "centroid x = {}", env.centroid[0]);
    assert!((env.centroid[1] - 50.0).abs() < 1.0, "centroid y = {}", env.centroid[1]);
    assert!(env.bbox.min_x <= 0.0 && env.bbox.max_x >= 100.0);
    assert!(env.bbox.min_y <= 0.0 && env.bbox.max_y >= 100.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn replication_emits_multipolygon_envelope_spanning_parts() {
    let (_fix, src) = boot().await;
    create_table(&src, "mp", "MultiPolygon", 25832).await;
    let src = PgSource::connect(pg_cfg(&_fix.dsn))
        .await
        .unwrap()
        .with_topology(topology("mp_c", "mp"));
    let mut sub = src.subscribe().await.unwrap();

    let writer = PgSource::connect(pg_cfg(&_fix.dsn)).await.unwrap();
    let c = writer.pool().get().await.unwrap();
    // two disjoint squares far apart; envelope must span the union.
    c.batch_execute(
        "INSERT INTO mp VALUES (1, ST_GeomFromText('MULTIPOLYGON(((0 0,10 0,10 10,0 10,0 0)),((900 900,910 900,910 910,900 910,900 900)))', 25832));",
    )
    .await
    .unwrap();

    let env = next_envelope(&mut sub).await;
    assert!(
        env.bbox.min_x <= 0.0 && env.bbox.max_x >= 910.0,
        "bbox x: {:?}",
        env.bbox
    );
    assert!(
        env.bbox.min_y <= 0.0 && env.bbox.max_y >= 910.0,
        "bbox y: {:?}",
        env.bbox
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn replication_rejects_geometrycollection_with_unsupported_type() {
    // GeometryCollection (WKB type 7) is not currently supported by the
    // bbox extractor at `mars-source-postgres` - this test locks in that
    // behavior. if support is later added, the assertion will fail and
    // someone has to deliberately update it.
    let (_fix, src) = boot().await;
    create_table(&src, "gc", "GeometryCollection", 25832).await;
    let src = PgSource::connect(pg_cfg(&_fix.dsn))
        .await
        .unwrap()
        .with_topology(topology("gc_c", "gc"));
    let mut sub = src.subscribe().await.unwrap();

    let writer = PgSource::connect(pg_cfg(&_fix.dsn)).await.unwrap();
    let c = writer.pool().get().await.unwrap();
    c.batch_execute(
        "INSERT INTO gc VALUES (1, ST_GeomFromText('GEOMETRYCOLLECTION(POINT(0 0), LINESTRING(50 50, 100 100))', 25832));",
    )
    .await
    .unwrap();

    let batch = tokio::time::timeout(Duration::from_secs(15), sub.next_batch())
        .await
        .expect("timeout")
        .expect("subscription closed");
    let err = batch.expect_err("GeometryCollection must currently surface as a backend error");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("wkb bbox") && msg.contains("UnsupportedType"),
        "expected wkb bbox UnsupportedType error, got {msg}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn replication_handles_non_default_srid_25833() {
    // SRID 25833 is the same UTM family, different zone. the adapter doesn't
    // reproject, so the emitted envelope is in the source SRID; we read the
    // raw coords back.
    let (_fix, src) = boot().await;
    create_table(&src, "srid", "Point", 25833).await;
    let src = PgSource::connect(pg_cfg(&_fix.dsn))
        .await
        .unwrap()
        .with_topology(topology("srid_c", "srid"));
    let mut sub = src.subscribe().await.unwrap();

    let writer = PgSource::connect(pg_cfg(&_fix.dsn)).await.unwrap();
    let c = writer.pool().get().await.unwrap();
    c.batch_execute("INSERT INTO srid VALUES (1, ST_SetSRID(ST_MakePoint(1234, 5678), 25833));")
        .await
        .unwrap();

    let env = next_envelope(&mut sub).await;
    assert!(
        (env.centroid[0] - 1234.0).abs() < 1.0 && (env.centroid[1] - 5678.0).abs() < 1.0,
        "centroid {:?} not at (1234, 5678)",
        env.centroid
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn replication_emits_large_polygon_envelope_without_truncation() {
    let (_fix, src) = boot().await;
    create_table(&src, "big", "Polygon", 25832).await;
    let src = PgSource::connect(pg_cfg(&_fix.dsn))
        .await
        .unwrap()
        .with_topology(topology("big_c", "big"));
    let mut sub = src.subscribe().await.unwrap();

    let writer = PgSource::connect(pg_cfg(&_fix.dsn)).await.unwrap();
    let c = writer.pool().get().await.unwrap();
    // construct ~5000-vertex polygon as a circle approximation. ST_Buffer on
    // a point with a high num_segments gives a dense ring.
    c.batch_execute("INSERT INTO big VALUES (1, ST_Buffer(ST_SetSRID(ST_MakePoint(500, 500), 25832), 400, 1250));")
        .await
        .unwrap();

    let env = next_envelope(&mut sub).await;
    // envelope of a buffered point centred at (500,500) radius 400 spans
    // ~(100..900) in both axes.
    assert!(
        env.bbox.min_x <= 110.0 && env.bbox.max_x >= 890.0,
        "bbox x: {:?}",
        env.bbox
    );
    assert!(
        env.bbox.min_y <= 110.0 && env.bbox.max_y >= 890.0,
        "bbox y: {:?}",
        env.bbox
    );
    assert!((env.centroid[0] - 500.0).abs() < 5.0);
    assert!((env.centroid[1] - 500.0).abs() < 5.0);
}

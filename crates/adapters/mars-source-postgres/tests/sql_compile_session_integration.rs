//! e2e: `Source::open_compile_session` against a `sql:` binding materialises
//! the inline SELECT into a per-session TEMP TABLE so pass-1 / pass-2 can use
//! `tableoid` / `ctid` for the snapshot-stable row identity. Verifies summary
//! and row counts agree across both passes and that the temp table dies with
//! the transaction.

#![cfg(feature = "integration")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use futures_util::StreamExt;
use mars_source::{Source, SourceBinding, SourceCollectionId};
use mars_source_postgres::{PgConfig, PgSource};
use mars_types::CrsCode;
use rand::distr::{Alphanumeric, SampleString};
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

const ROW_COUNT: i64 = 50;

#[tokio::test]
async fn sql_binding_compile_session_streams_pass1_and_pass2() {
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
        .expect("docker available");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let dsn = format!("host=127.0.0.1 port={port} user=mars password={password} dbname=mars");

    let (client, conn) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    // two source tables; the inline SELECT UNIONs them so the binding cannot
    // be expressed as a single table reference and the temp-table path is
    // the only thing that makes pass-1/pass-2 work.
    client
        .batch_execute(
            "CREATE EXTENSION IF NOT EXISTS postgis;
             CREATE TABLE a (
                gid bigint primary key,
                geom geometry(Point, 25832),
                name text
             );
             CREATE TABLE b (
                gid bigint primary key,
                geom geometry(Point, 25832),
                name text
             );",
        )
        .await
        .unwrap();
    for i in 0..ROW_COUNT {
        let x = f64::from(i as i32) * 10.0;
        let y = f64::from(i as i32) * 5.0;
        let name = format!("a-{i}");
        client
            .execute(
                "INSERT INTO a (gid, geom, name) VALUES ($1, ST_SetSRID(ST_MakePoint($2, $3), 25832), $4)",
                &[&i, &x, &y, &name],
            )
            .await
            .unwrap();
    }
    for i in 0..ROW_COUNT {
        let id = i + ROW_COUNT;
        let x = f64::from(i as i32) * 11.0;
        let y = f64::from(i as i32) * 6.0;
        let name = format!("b-{i}");
        client
            .execute(
                "INSERT INTO b (gid, geom, name) VALUES ($1, ST_SetSRID(ST_MakePoint($2, $3), 25832), $4)",
                &[&id, &x, &y, &name],
            )
            .await
            .unwrap();
    }

    let cfg = PgConfig {
        dsn,
        publication: String::new(),
        slot: String::new(),
        ..Default::default()
    };
    let src = PgSource::connect(cfg).await.unwrap();
    let binding = SourceBinding::new(
        SourceCollectionId::new("ab_union"),
        "(SELECT gid, geom, name FROM a UNION ALL SELECT gid, geom, name FROM b)",
        "geom",
        "gid",
        vec!["name".into()],
        CrsCode::new("EPSG:25832"),
    )
    .unwrap();

    let mut session = src.open_compile_session(&binding).await.unwrap();

    let mut pass1_ids: BTreeSet<i64> = BTreeSet::new();
    {
        let mut stream = session.stream_geometry_summary().await.unwrap();
        while let Some(item) = stream.next().await {
            let row = item.unwrap();
            assert!(pass1_ids.insert(row.feature_id));
        }
    }
    assert_eq!(pass1_ids.len(), (ROW_COUNT * 2) as usize);

    let mut pass2_ids: BTreeSet<u64> = BTreeSet::new();
    {
        let mut stream = session.stream_rows().await.unwrap();
        while let Some(item) = stream.next().await {
            let row = item.unwrap();
            assert!(pass2_ids.insert(row.feature_id));
            assert!(!row.geometry.is_empty());
        }
    }
    assert_eq!(pass2_ids.len(), (ROW_COUNT * 2) as usize);
    for id in &pass1_ids {
        assert!(pass2_ids.contains(&(*id as u64)));
    }

    session.commit().await.unwrap();
}

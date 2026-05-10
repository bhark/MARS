//! e2e: `fetch_full_table_streaming` against a postgis container streams every
//! row of a synthetic table without buffering, irrespective of physical order.

#![cfg(feature = "e2e")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use futures_util::StreamExt;
use mars_source::{Source, SourceBinding, SourceCollectionId};
use mars_source_postgres::{PgConfig, PgSource};
use mars_types::CrsCode;
use rand::distributions::{Alphanumeric, DistString};
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

const ROW_COUNT: i64 = 100;

#[tokio::test]
async fn fetch_full_table_streaming_yields_every_row() {
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
        .expect("docker available");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let dsn = format!("host=127.0.0.1 port={port} user=mars password={password} dbname=mars");

    // populate a synthetic table.
    let (client, conn) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
        .batch_execute(
            "CREATE EXTENSION IF NOT EXISTS postgis;
             CREATE TABLE rows (
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
        let name = format!("row-{i}");
        client
            .execute(
                "INSERT INTO rows (gid, geom, name) VALUES ($1, ST_SetSRID(ST_MakePoint($2, $3), 25832), $4)",
                &[&i, &x, &y, &name],
            )
            .await
            .unwrap();
    }

    // stream via the adapter.
    let cfg = PgConfig {
        dsn,
        publication: String::new(),
        slot: String::new(),
        ..Default::default()
    };
    let src = PgSource::connect(cfg).await.unwrap();
    let binding = SourceBinding::new(
        SourceCollectionId::new("rows"),
        "public",
        "rows",
        "geom",
        "gid",
        vec!["name".into()],
        CrsCode::new("EPSG:25832"),
    )
    .unwrap();

    let mut stream = src.fetch_full_table_streaming(&binding).await.unwrap();
    let mut seen_ids: BTreeSet<u64> = BTreeSet::new();
    while let Some(item) = stream.next().await {
        let row = item.unwrap();
        assert!(
            seen_ids.insert(row.feature_id),
            "duplicate feature_id {}",
            row.feature_id
        );
        assert!(!row.geometry.is_empty(), "empty wkb for {}", row.feature_id);
        assert_eq!(row.attributes.len(), 1);
        assert_eq!(row.attributes[0].0, "name");
    }
    assert_eq!(seen_ids.len(), ROW_COUNT as usize);
    assert_eq!(*seen_ids.first().unwrap(), 0);
    assert_eq!(*seen_ids.last().unwrap(), (ROW_COUNT - 1) as u64);
}

#[tokio::test]
async fn fetch_full_table_streaming_skips_null_geom_rows() {
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
        .expect("docker available");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let dsn = format!("host=127.0.0.1 port={port} user=mars password={password} dbname=mars");

    let (client, conn) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
        .batch_execute(
            "CREATE EXTENSION IF NOT EXISTS postgis;
             CREATE TABLE rows (
                gid bigint primary key,
                geom geometry(Point, 25832),
                name text
             );
             INSERT INTO rows (gid, geom, name) VALUES
                (1, ST_SetSRID(ST_MakePoint(0, 0), 25832), 'kept'),
                (2, NULL, 'skipped'),
                (3, ST_SetSRID(ST_MakePoint(1, 1), 25832), 'kept');",
        )
        .await
        .unwrap();

    let cfg = PgConfig {
        dsn,
        publication: String::new(),
        slot: String::new(),
        ..Default::default()
    };
    let src = PgSource::connect(cfg).await.unwrap();
    let binding = SourceBinding::new(
        SourceCollectionId::new("rows"),
        "public",
        "rows",
        "geom",
        "gid",
        vec!["name".into()],
        CrsCode::new("EPSG:25832"),
    )
    .unwrap();

    let mut stream = src.fetch_full_table_streaming(&binding).await.unwrap();
    let mut seen_ids: BTreeSet<u64> = BTreeSet::new();
    while let Some(item) = stream.next().await {
        let row = item.unwrap();
        seen_ids.insert(row.feature_id);
    }
    assert_eq!(seen_ids, BTreeSet::from([1u64, 3u64]));
}

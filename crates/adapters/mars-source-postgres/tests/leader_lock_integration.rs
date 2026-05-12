//! e2e: two `PgSource::try_acquire` calls against the same postgres produce
//! exactly one leader; dropping the guard releases the lock so a third call
//! succeeds.

#![cfg(feature = "integration")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mars_source::LeaderLock;
use mars_source_postgres::{PgConfig, PgSource};
use rand::distr::{Alphanumeric, SampleString};
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

#[tokio::test]
async fn try_acquire_grants_one_leader_and_denies_concurrent() {
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

    let cfg = PgConfig {
        dsn,
        publication: String::new(),
        slot: String::new(),
        ..Default::default()
    };
    let a = PgSource::connect(cfg.clone()).await.unwrap();
    let b = PgSource::connect(cfg).await.unwrap();

    let key: i64 = 0x4d41_5253_5f4c_434b; // arbitrary; both callers must agree

    let g1 = a.try_acquire(key).await.unwrap();
    assert!(g1.is_some(), "first acquire must succeed");

    let g2 = b.try_acquire(key).await.unwrap();
    assert!(g2.is_none(), "second concurrent acquire must be denied");

    drop(g1);
    // give the unlock task a moment to run; advisory unlock is async via the
    // detached task in the guard's Drop impl.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let g3 = b.try_acquire(key).await.unwrap();
    assert!(g3.is_some(), "after release a fresh acquire must succeed");
}

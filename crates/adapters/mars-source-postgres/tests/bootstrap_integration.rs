//! e2e: catalog provisioning against a postgis container.
//!
//! Covers what the `mars setup` / `mars teardown` CLI surfaces ultimately
//! drive: idempotent role / grants / publication / slot creation, and
//! schema-set reconciliation when the bootstrap is re-applied with a
//! different schemas list.

#![cfg(feature = "integration")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mars_source_postgres::bootstrap::{BootstrapPlan, TeardownPlan, apply, teardown};
use rand::distr::{Alphanumeric, SampleString};
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use tokio_postgres::NoTls;

const ROLE: &str = "mars_replicator";
const PUB: &str = "mars_bs_pub";
const SLOT: &str = "mars_bs_slot";

async fn boot_postgis() -> (ContainerAsync<GenericImage>, String) {
    let password = Alphanumeric.sample_string(&mut rand::rng(), 16);
    let container = GenericImage::new("postgis/postgis", "16-3.4")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_PASSWORD", &password)
        .with_env_var("POSTGRES_USER", "admin")
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
        .expect("docker available");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let dsn = format!("host=127.0.0.1 port={port} user=admin password={password} dbname=mars sslmode=disable");
    (container, dsn)
}

async fn connect(dsn: &str) -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(dsn, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
}

async fn create_schemas(client: &tokio_postgres::Client, schemas: &[&str]) {
    for s in schemas {
        client
            .batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{s}\";"))
            .await
            .unwrap();
        client
            .batch_execute(&format!(
                "CREATE TABLE IF NOT EXISTS \"{s}\".\"things\" (id INT PRIMARY KEY, payload TEXT);"
            ))
            .await
            .unwrap();
    }
}

async fn role_exists(client: &tokio_postgres::Client, role: &str) -> bool {
    client
        .query_one("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)", &[&role])
        .await
        .unwrap()
        .get(0)
}

async fn publication_schemas(client: &tokio_postgres::Client, pubname: &str) -> Vec<String> {
    let rows = client
        .query(
            "SELECT n.nspname FROM pg_publication_namespace pn \
             JOIN pg_namespace n ON n.oid = pn.pnnspid \
             JOIN pg_publication p ON p.oid = pn.pnpubid \
             WHERE p.pubname = $1 ORDER BY n.nspname",
            &[&pubname],
        )
        .await
        .unwrap();
    rows.into_iter().map(|r| r.get::<_, String>(0)).collect()
}

async fn slot_exists(client: &tokio_postgres::Client, slot: &str) -> bool {
    client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_replication_slots WHERE slot_name = $1)",
            &[&slot],
        )
        .await
        .unwrap()
        .get(0)
}

async fn has_select(client: &tokio_postgres::Client, role: &str, schema: &str, table: &str) -> bool {
    client
        .query_one(
            "SELECT has_table_privilege($1, format('%I.%I', $2::text, $3::text), 'SELECT')",
            &[&role, &schema, &table],
        )
        .await
        .unwrap()
        .get(0)
}

fn plan(schemas: &[&str]) -> BootstrapPlan {
    BootstrapPlan {
        role: ROLE.into(),
        runtime_password: "s3cret".into(),
        publication: PUB.into(),
        slot: SLOT.into(),
        schemas: schemas.iter().map(|s| (*s).to_string()).collect(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn applies_role_grants_publication_and_slot() {
    let (_container, dsn) = boot_postgis().await;
    let admin = connect(&dsn).await;
    create_schemas(&admin, &["app", "geo"]).await;

    apply(&dsn, &plan(&["app", "geo"])).await.expect("apply");

    assert!(role_exists(&admin, ROLE).await);
    assert_eq!(publication_schemas(&admin, PUB).await, vec!["app", "geo"]);
    assert!(slot_exists(&admin, SLOT).await);
    assert!(has_select(&admin, ROLE, "app", "things").await);
    assert!(has_select(&admin, ROLE, "geo", "things").await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn applying_twice_is_a_no_op() {
    let (_container, dsn) = boot_postgis().await;
    let admin = connect(&dsn).await;
    create_schemas(&admin, &["app"]).await;

    apply(&dsn, &plan(&["app"])).await.expect("first apply");
    apply(&dsn, &plan(&["app"]))
        .await
        .expect("second apply should be idempotent");

    assert_eq!(publication_schemas(&admin, PUB).await, vec!["app"]);
    assert!(slot_exists(&admin, SLOT).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schema_mutation_reconciles_publication() {
    let (_container, dsn) = boot_postgis().await;
    let admin = connect(&dsn).await;
    create_schemas(&admin, &["app", "geo", "extra"]).await;

    apply(&dsn, &plan(&["app", "geo"])).await.expect("initial apply");
    assert_eq!(publication_schemas(&admin, PUB).await, vec!["app", "geo"]);

    apply(&dsn, &plan(&["app", "extra"])).await.expect("reconcile apply");

    assert_eq!(publication_schemas(&admin, PUB).await, vec!["app", "extra"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teardown_drops_per_flag() {
    let (_container, dsn) = boot_postgis().await;
    let admin = connect(&dsn).await;
    create_schemas(&admin, &["app"]).await;

    apply(&dsn, &plan(&["app"])).await.unwrap();

    // partial teardown: slot only
    teardown(
        &dsn,
        &TeardownPlan {
            role: ROLE.into(),
            publication: PUB.into(),
            slot: SLOT.into(),
            drop_slot: true,
            drop_publication: false,
            drop_role: false,
        },
    )
    .await
    .unwrap();
    assert!(!slot_exists(&admin, SLOT).await);
    assert_eq!(publication_schemas(&admin, PUB).await, vec!["app"]);
    assert!(role_exists(&admin, ROLE).await);

    // remaining teardown: publication + role.
    // grants are dropped along with the role automatically by postgres.
    admin
        .batch_execute(&format!(
            "REVOKE USAGE ON SCHEMA \"app\" FROM \"{ROLE}\"; \
             REVOKE SELECT ON ALL TABLES IN SCHEMA \"app\" FROM \"{ROLE}\"; \
             ALTER DEFAULT PRIVILEGES IN SCHEMA \"app\" REVOKE SELECT ON TABLES FROM \"{ROLE}\";"
        ))
        .await
        .unwrap();
    teardown(
        &dsn,
        &TeardownPlan {
            role: ROLE.into(),
            publication: PUB.into(),
            slot: SLOT.into(),
            drop_slot: false,
            drop_publication: true,
            drop_role: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(publication_schemas(&admin, PUB).await, Vec::<String>::new());
    assert!(!role_exists(&admin, ROLE).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teardown_tolerates_missing_objects() {
    let (_container, dsn) = boot_postgis().await;
    teardown(
        &dsn,
        &TeardownPlan {
            role: ROLE.into(),
            publication: PUB.into(),
            slot: SLOT.into(),
            drop_slot: true,
            drop_publication: true,
            drop_role: true,
        },
    )
    .await
    .expect("teardown of empty catalog is a no-op");
}

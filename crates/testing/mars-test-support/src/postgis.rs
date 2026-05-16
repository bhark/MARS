//! postgis testcontainer bring-up for integration tests.
//!
//! pattern lifted from `crates/adapters/mars-source-postgres/tests/replication_integration.rs`
//! and `bootstrap_integration.rs`; the two sites differ only in `POSTGRES_USER`.
//! `PostgisOptions` parameterizes that without forcing each call site to
//! re-declare the rest of the container shape.

use rand::distr::{Alphanumeric, SampleString};
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

/// fully booted postgis instance. `_container` is held to keep the container
/// alive for the lifetime of the fixture; drop the fixture to stop it.
pub struct PostgisFixture {
    #[allow(dead_code)]
    container: ContainerAsync<GenericImage>,
    pub dsn: String,
    pub user: String,
    pub password: String,
    pub host: String,
    pub port: u16,
    pub database: String,
}

/// overrides for the default postgis container shape. all fields have working
/// defaults via `PostgisOptions::default()`.
#[derive(Debug, Clone)]
pub struct PostgisOptions {
    /// `POSTGRES_USER`. defaults to `"mars"`.
    pub user: String,
    /// `POSTGRES_DB`. defaults to `"mars"`.
    pub database: String,
    /// image tag for `postgis/postgis`. defaults to `"16-3.4"`.
    pub image_tag: String,
    /// when `Some`, appended to the DSN as `sslmode=<v>`. defaults to `None`.
    pub sslmode: Option<String>,
}

impl Default for PostgisOptions {
    fn default() -> Self {
        Self {
            user: "mars".into(),
            database: "mars".into(),
            image_tag: "16-3.4".into(),
            sslmode: None,
        }
    }
}

/// boot a postgis container with the default shape (`POSTGRES_USER=mars`,
/// `wal_level=logical`, replication knobs raised for logical decoding).
///
/// panics on docker bring-up failure; tests assume docker is available when
/// `--features integration` is set.
pub async fn boot_postgis() -> PostgisFixture {
    boot_postgis_with(PostgisOptions::default()).await
}

/// boot a postgis container with overridden user/db/tag/sslmode. logical
/// replication knobs are always set; callers who don't need replication just
/// ignore them.
pub async fn boot_postgis_with(opts: PostgisOptions) -> PostgisFixture {
    let password = Alphanumeric.sample_string(&mut rand::rng(), 16);
    let container = GenericImage::new("postgis/postgis", &opts.image_tag)
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_PASSWORD", &password)
        .with_env_var("POSTGRES_USER", &opts.user)
        .with_env_var("POSTGRES_DB", &opts.database)
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
        .expect("docker available; postgis container starts");
    let port = container.get_host_port_ipv4(5432).await.expect("postgis exposes 5432");
    let host = "127.0.0.1".to_string();
    let mut dsn = format!(
        "host={host} port={port} user={user} password={password} dbname={db}",
        user = opts.user,
        db = opts.database,
    );
    if let Some(mode) = &opts.sslmode {
        dsn.push_str(&format!(" sslmode={mode}"));
    }
    PostgisFixture {
        container,
        dsn,
        user: opts.user,
        password,
        host,
        port,
        database: opts.database,
    }
}

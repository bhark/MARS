//! PostgreSQL adapter for `mars-source`.
//!
//! Two strategies behind the same `ChangeFeed` trait:
//! - `pgoutput` logical decoding (default; SPEC §8.2.1) — Phase 1.
//! - Polling fallback under the `polling` feature (SPEC §8.2.2; second-class).
//!
//! This crate also owns the lowering of `mars-expr::Expr` to a parameterised
//! SQL `WHERE` clause. The lowering lives here, not in `mars-expr`, because
//! database vocabulary belongs in the database adapter and parameterisation
//! is enforceable in one place.

#![forbid(unsafe_code)]

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use deadpool_postgres::{Config as PoolConfig, Pool, Runtime};
use futures_core::stream::BoxStream;
use mars_expr::Expr;
use mars_source::{ChangeEvent, ChangeFeed, RowBytes, Source, SourceBinding, SourceError};
use mars_types::{Bbox, Cell};
use tokio_postgres::NoTls;

mod fetch;
mod leader;
mod lower;
mod quote;
mod replication;

pub use lower::lower_to_sql;
pub use replication::{CollectionTopology, ReplicationTopology};

/// Connection / topology configuration.
#[derive(Clone, Default)]
pub struct PgConfig {
    /// libpq DSN.
    pub dsn: String,
    /// Logical replication publication name.
    pub publication: String,
    /// Logical replication slot name.
    pub slot: String,
}

impl std::fmt::Debug for PgConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgConfig")
            .field("dsn", &redact_dsn(&self.dsn))
            .field("publication", &self.publication)
            .field("slot", &self.slot)
            .finish()
    }
}

fn redact_dsn(dsn: &str) -> String {
    if dsn.contains("://") {
        // URI form: postgresql://user:password@host/...
        if let Some(at) = dsn.find('@')
            && let Some(scheme_end) = dsn.find("://")
        {
            return format!("{}user:***@{}", &dsn[..scheme_end + 3], &dsn[at + 1..]);
        }
        return dsn.to_string();
    }

    // key-value form
    dsn.split(' ')
        .map(|part| {
            if let Some(eq) = part.find('=') {
                let key = &part[..eq];
                if key.eq_ignore_ascii_case("password") || key.eq_ignore_ascii_case("passwd") {
                    return format!("{key}=***");
                }
            }
            part.to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Real PostgreSQL/PostGIS adapter. Holds a `deadpool` pool over `tokio-postgres`.
#[derive(Debug)]
pub struct PgSource {
    pool: Pool,
    cfg: Arc<PgConfig>,
    topology: Option<Arc<ReplicationTopology>>,
}

impl PgSource {
    /// Connect (open the pool) using the supplied DSN. Does not establish
    /// any connection up front; the first `fetch_cell` call will.
    pub async fn connect(cfg: PgConfig) -> Result<Self, SourceError> {
        let pg_cfg =
            tokio_postgres::Config::from_str(&cfg.dsn).map_err(|e| SourceError::Backend(format!("dsn: {e}")))?;
        let mut pool_cfg = PoolConfig::new();
        pool_cfg.host = pg_cfg.get_hosts().first().and_then(|h| match h {
            tokio_postgres::config::Host::Tcp(s) => Some(s.clone()),
            #[cfg(unix)]
            tokio_postgres::config::Host::Unix(p) => p.to_str().map(str::to_owned),
        });
        pool_cfg.port = pg_cfg.get_ports().first().copied();
        pool_cfg.user = pg_cfg.get_user().map(str::to_owned);
        pool_cfg.password = pg_cfg.get_password().map(|b| String::from_utf8_lossy(b).into_owned());
        pool_cfg.dbname = pg_cfg.get_dbname().map(str::to_owned);

        let pool = pool_cfg
            .create_pool(Some(Runtime::Tokio1), NoTls)
            .map_err(|e| SourceError::Backend(format!("pool create: {e}")))?;

        Ok(Self {
            pool,
            cfg: Arc::new(cfg),
            topology: None,
        })
    }

    /// Wire the replication topology used by `subscribe`. The topology is
    /// derived from the parsed `mars-config` (collection -> table mapping +
    /// scale bands) at bin-composition time. Without it `subscribe`
    /// returns `InvalidBinding`.
    #[must_use]
    pub fn with_topology(mut self, topology: ReplicationTopology) -> Self {
        self.topology = Some(Arc::new(topology));
        self
    }

    /// Borrow the underlying pool — useful for tests and future extensions
    /// (e.g. per-call statement cache, replication cursors).
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Borrow the original config.
    #[must_use]
    pub fn config(&self) -> &PgConfig {
        &self.cfg
    }
}

#[async_trait]
impl Source for PgSource {
    async fn fetch_cell(
        &self,
        binding: &SourceBinding,
        _cell: &Cell,
        bbox: Bbox,
        filter: Option<&Expr>,
    ) -> Result<Vec<RowBytes>, SourceError> {
        fetch::fetch_cell(&self.pool, binding, bbox, filter).await
    }
}

#[async_trait]
impl ChangeFeed for PgSource {
    async fn subscribe(&self) -> Result<BoxStream<'static, Result<ChangeEvent, SourceError>>, SourceError> {
        let topology = self.topology.clone().ok_or_else(|| {
            SourceError::InvalidBinding("ReplicationTopology not wired; call PgSource::with_topology".into())
        })?;
        replication::subscribe(self.cfg.clone(), topology).await
    }
}

/// Bind parameter for a lowered SQL fragment. Decoupled from `tokio-postgres`
/// so unit tests can inspect the parameter list without any DB.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlParam {
    /// SQL NULL — currently produced only by callers that explicitly bind it.
    Null,
    /// Boolean.
    Bool(bool),
    /// 64-bit signed integer.
    Int(i64),
    /// 64-bit float.
    Float(f64),
    /// UTF-8 text.
    Text(String),
}

#[cfg(all(test, feature = "e2e"))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod e2e_tests {
    use super::*;
    use bytes::Bytes;
    use mars_source::SourceCollectionId;
    use mars_types::{CrsCode, ScaleBand};
    use rand::distributions::{Alphanumeric, DistString};
    use testcontainers::{
        GenericImage, ImageExt,
        core::{IntoContainerPort, WaitFor},
        runners::AsyncRunner,
    };

    #[tokio::test]
    async fn fetch_cell_returns_five_rows() {
        let _ = Bytes::new(); // ensure bytes is used in this cfg

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

        // setup table
        let setup = PgConfig {
            dsn: dsn.clone(),
            publication: String::new(),
            slot: String::new(),
        };
        let src = PgSource::connect(setup).await.unwrap();
        let client = src.pool.get().await.unwrap();
        client
            .batch_execute(
                "CREATE EXTENSION IF NOT EXISTS postgis;
                 CREATE TABLE t (
                    gid INT4 PRIMARY KEY,
                    name TEXT,
                    kind INT4,
                    geom geometry(Polygon, 25832)
                 );
                 INSERT INTO t VALUES
                    (1, 'a', 10, ST_GeomFromText('POLYGON((0 0,10 0,10 10,0 10,0 0))', 25832)),
                    (2, 'b', 20, ST_GeomFromText('POLYGON((20 0,30 0,30 10,20 10,20 0))', 25832)),
                    (3, 'c', 30, ST_GeomFromText('POLYGON((40 0,50 0,50 10,40 10,40 0))', 25832)),
                    (4, 'd', 40, ST_GeomFromText('POLYGON((60 0,70 0,70 10,60 10,60 0))', 25832)),
                    (5, 'e', 50, ST_GeomFromText('POLYGON((80 0,90 0,90 10,80 10,80 0))', 25832));",
            )
            .await
            .unwrap();
        drop(client);

        let binding = SourceBinding::new(
            SourceCollectionId::new("c"),
            "public",
            "t",
            "geom",
            "gid",
            vec!["name".into(), "kind".into()],
            CrsCode::new("EPSG:25832"),
        )
        .unwrap();
        let cell = Cell {
            band: ScaleBand::new("hi"),
            x: 0,
            y: 0,
        };
        let bbox = Bbox::new(-1.0, -1.0, 1000.0, 1000.0);

        let rows = src.fetch_cell(&binding, &cell, bbox, None).await.unwrap();
        assert_eq!(rows.len(), 5);
        for row in &rows {
            assert!(!row.geometry.is_empty());
            assert_eq!(row.attributes.len(), 2);
            assert_eq!(row.attributes[0].0, "name");
            assert_eq!(row.attributes[1].0, "kind");
        }
    }
}

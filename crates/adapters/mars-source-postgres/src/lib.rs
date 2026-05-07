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
use std::time::Duration;

use async_trait::async_trait;
use deadpool_postgres::{Hook, HookError, Pool, Runtime};
use mars_expr::Expr;
use mars_source::{ChangeFeed, ChangeSubscription, RowBytes, Source, SourceBinding, SourceError};
use mars_types::{Bbox, Cell};
use tokio_postgres::NoTls;

mod fetch;
mod leader;
mod lower;
mod quote;
mod replication;

pub use lower::lower_to_sql;
pub use mars_source::SourceCollectionId;
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
    /// Maximum pool size; falls back to deadpool defaults when `None`.
    pub max_pool_size: Option<usize>,
    /// Per-connection idle recycle timeout. Connections idle past this age are
    /// dropped on next checkout.
    pub recycle_timeout: Option<Duration>,
    /// Per-statement timeout applied via `SET statement_timeout` on every
    /// checkout. `None` leaves the server default in place.
    pub statement_timeout: Option<Duration>,
    /// Bound on the number of concurrent in-flight queries pipelined on a
    /// single connection inside `fetch_cells`. Falls back to a small default
    /// when `None`. Higher values amortise RTT but stack response buffers.
    pub fetch_concurrency: Option<usize>,
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
        let mut s = dsn.to_string();
        // authority section: user:password@host
        if let Some(at) = s.find('@')
            && let Some(scheme_end) = s.find("://")
        {
            s.replace_range(scheme_end + 3..at, "user:***");
        }
        // query-string or key-value params embedded in URI
        s = redact_value(s, "password");
        s = redact_value(s, "passwd");
        return s;
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

/// replace every `key=<value>` in `s` with `key=***` (case-insensitive key).
fn redact_value(mut s: String, key: &str) -> String {
    let prefix = format!("{key}=");
    let mut idx = 0;
    while let Some(pos) = s[idx..].to_lowercase().find(&prefix) {
        let start = idx + pos + prefix.len();
        let end = s[start..]
            .find(&['&', ';', ' '][..])
            .map(|p| start + p)
            .unwrap_or(s.len());
        s.replace_range(start..end, "***");
        idx = start + 3;
    }
    s
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

        let mgr_cfg = deadpool_postgres::ManagerConfig::default();
        let mgr = if pg_cfg.get_ssl_mode() == tokio_postgres::config::SslMode::Disable {
            deadpool_postgres::Manager::from_config(pg_cfg, NoTls, mgr_cfg)
        } else {
            // install_default returns Err only when a provider is already
            // installed in this process — benign when the host wired one up
            // already, suspicious only on first call. surface errors from
            // root loading, since silently trusting an empty store would
            // make every TLS connect fail with an opaque protocol error.
            let _ = rustls::crypto::ring::default_provider().install_default();
            let load = rustls_native_certs::load_native_certs();
            if !load.errors.is_empty() {
                let summary: Vec<String> = load.errors.iter().take(3).map(|e| e.to_string()).collect();
                tracing::warn!(
                    errors = ?summary,
                    total = load.errors.len(),
                    "rustls-native-certs partial load",
                );
            }
            if load.certs.is_empty() {
                return Err(SourceError::Backend(
                    "no native trust roots available; refusing to connect with empty trust store".into(),
                ));
            }
            let mut roots = rustls::RootCertStore::empty();
            for cert in load.certs {
                let _ = roots.add(cert);
            }
            let tls_cfg = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let tls = tokio_postgres_rustls::MakeRustlsConnect::new(tls_cfg);
            deadpool_postgres::Manager::from_config(pg_cfg, tls, mgr_cfg)
        };

        let mut builder = Pool::builder(mgr).runtime(Runtime::Tokio1);
        if let Some(n) = cfg.max_pool_size {
            builder = builder.max_size(n);
        }
        if let Some(d) = cfg.recycle_timeout {
            builder = builder.recycle_timeout(Some(d));
        }
        if let Some(timeout) = cfg.statement_timeout {
            // post_create hook fires once per fresh connection. set
            // statement_timeout there so every checkout sees the bound.
            let ms = timeout.as_millis() as u64;
            let stmt = format!("SET statement_timeout = {ms}");
            builder = builder.post_create(Hook::async_fn(move |client, _| {
                let stmt = stmt.clone();
                Box::pin(async move {
                    client
                        .batch_execute(&stmt)
                        .await
                        .map_err(|e| HookError::message(format!("statement_timeout: {e}")))
                })
            }));
        }
        let pool = builder
            .build()
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

    async fn fetch_cells(
        &self,
        binding: &SourceBinding,
        cells: &[(Cell, Bbox)],
        filter: Option<&Expr>,
    ) -> Result<Vec<(Cell, Vec<RowBytes>)>, SourceError> {
        let concurrency = self.cfg.fetch_concurrency.unwrap_or(fetch::DEFAULT_FETCH_CONCURRENCY);
        fetch::fetch_cells(&self.pool, binding, cells, filter, concurrency).await
    }
}

#[async_trait]
impl ChangeFeed for PgSource {
    async fn subscribe(&self) -> Result<Box<dyn ChangeSubscription>, SourceError> {
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
            ..Default::default()
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

        // pipelined fetch_cells over three disjoint bboxes; row sets must match
        // serial fetch_cell calls and routing must pair each output with its
        // input cell.
        let band = ScaleBand::new("hi");
        let mk_cell = |x| Cell {
            band: band.clone(),
            x,
            y: 0,
        };
        let cells = vec![
            (mk_cell(0), Bbox::new(-1.0, -1.0, 15.0, 20.0)),
            (mk_cell(1), Bbox::new(35.0, -1.0, 55.0, 20.0)),
            (mk_cell(2), Bbox::new(75.0, -1.0, 95.0, 20.0)),
        ];

        let batched = src.fetch_cells(&binding, &cells, None).await.unwrap();
        assert_eq!(batched.len(), 3);

        for ((in_cell, bbox), (out_cell, rows)) in cells.iter().zip(&batched) {
            assert_eq!(in_cell, out_cell);
            let serial = src.fetch_cell(&binding, in_cell, *bbox, None).await.unwrap();
            assert_eq!(rows.len(), serial.len());
            let mut got: Vec<u64> = rows.iter().map(|r| r.feature_id).collect();
            let mut want: Vec<u64> = serial.iter().map(|r| r.feature_id).collect();
            got.sort_unstable();
            want.sort_unstable();
            assert_eq!(got, want);
        }
    }
}

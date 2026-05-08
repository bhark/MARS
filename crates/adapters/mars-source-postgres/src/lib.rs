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
use futures_core::stream::BoxStream;
use mars_source::{ChangeFeed, ChangeSubscription, RowBytes, Source, SourceBinding, SourceError};
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
    /// Maximum time the replication worker will wait for the consumer to
    /// accept a committed batch before aborting the subscription. Past this
    /// budget the slot would pin and pg WAL would accumulate without bound.
    /// Falls back to a sane default when `None`.
    pub batch_send_timeout: Option<Duration>,
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
        let pg_cfg = tokio_postgres::Config::from_str(&cfg.dsn).map_err(|e| SourceError::backend("dsn", e))?;

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
                return Err(SourceError::backend_msg(
                    "tls",
                    "no native trust roots available; refusing to connect with empty trust store",
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
        let pool = builder.build().map_err(|e| SourceError::backend("pool create", e))?;

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
    async fn fetch_full_table_streaming<'a>(
        &'a self,
        binding: &'a SourceBinding,
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        fetch::fetch_full_table_streaming(self.pool.clone(), binding.clone()).await
    }

    async fn fetch_by_feature_ids<'a>(
        &'a self,
        binding: &'a SourceBinding,
        ids: &'a [i64],
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        fetch::fetch_by_feature_ids(self.pool.clone(), binding.clone(), ids.to_vec()).await
    }

    async fn stream_feature_ids<'a>(
        &'a self,
        binding: &'a SourceBinding,
    ) -> Result<BoxStream<'a, Result<i64, SourceError>>, SourceError> {
        fetch::stream_feature_ids(self.pool.clone(), binding.clone()).await
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

// phase-c: cell-keyed e2e tests retired with the v3 substrate cut. Phase D
// reintroduces page-keyed e2e coverage in `crates/bin/mars` under the
// `e2e` feature.

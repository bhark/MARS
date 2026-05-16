//! PostgreSQL adapter for `mars-source`.
//!
//! Change feed via `pgoutput` logical decoding.
//!
//! This crate also owns the lowering of `mars-expr::Expr` to a parameterised
//! SQL `WHERE` clause. The lowering lives here, not in `mars-expr`, because
//! database vocabulary belongs in the database adapter and parameterisation
//! is enforceable in one place.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use deadpool_postgres::{Hook, HookError, Pool, Runtime};
use futures_core::stream::BoxStream;
use mars_source::{
    BindingHealth, ChangeFeed, ChangeSubscription, CompileSession, RowBytes, Source, SourceBinding, SourceError,
};
use tokio_postgres::NoTls;

pub mod bootstrap;
mod compile_session;
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
    /// libpq DSN. The source-scope default; owns the logical replication
    /// slot.
    pub dsn: String,
    /// Additional DSNs reachable via per-binding overrides. The adapter
    /// pre-builds one pool per unique DSN at `PgSource::connect` time and
    /// routes each call through `pool_for`. Override bindings are
    /// snapshot-only - the replication slot lives on `dsn` and only sees
    /// changes on that database.
    pub binding_dsns: Vec<String>,
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

/// Real PostgreSQL/PostGIS adapter. Holds one `deadpool` pool for the
/// source-scope DSN (also owns logical replication) plus one extra pool per
/// distinct per-binding override DSN declared on the config. Override pools
/// serve snapshot/rebuild queries only.
#[derive(Debug)]
pub struct PgSource {
    default_pool: Pool,
    override_pools: BTreeMap<String, Pool>,
    cfg: Arc<PgConfig>,
    topology: Option<Arc<ReplicationTopology>>,
}

impl PgSource {
    /// Connect (open the pools) using the supplied DSN list. Does not
    /// establish any connection up front; the first query against each pool
    /// will. One pool is built per unique DSN across `cfg.dsn` +
    /// `cfg.binding_dsns`.
    pub async fn connect(cfg: PgConfig) -> Result<Self, SourceError> {
        let default_pool = build_pool(&cfg.dsn, &cfg)?;
        let mut override_pools = BTreeMap::new();
        let mut overrides: Vec<&str> = cfg.binding_dsns.iter().map(String::as_str).collect();
        overrides.sort();
        overrides.dedup();
        for dsn in overrides {
            if dsn == cfg.dsn {
                // a binding override that matches the source-scope DSN routes
                // to the default pool; no separate pool needed.
                continue;
            }
            override_pools.insert(dsn.to_string(), build_pool(dsn, &cfg)?);
        }

        Ok(Self {
            default_pool,
            override_pools,
            cfg: Arc::new(cfg),
            topology: None,
        })
    }

    /// Pick the pool serving this binding: the override DSN carried on the
    /// binding when set and registered at connect time, otherwise the
    /// source-scope default.
    fn pool_for(&self, binding: &SourceBinding) -> Result<Pool, SourceError> {
        let Some(dsn) = binding.dsn.as_deref() else {
            return Ok(self.default_pool.clone());
        };
        if dsn == self.cfg.dsn {
            return Ok(self.default_pool.clone());
        }
        self.override_pools
            .get(dsn)
            .cloned()
            .ok_or_else(|| SourceError::InvalidBinding(format!("no pool wired for binding dsn override {dsn:?}")))
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

    /// Borrow the source-scope (default) pool. Used by code paths that
    /// aren't per-binding (catalog probes, replication setup).
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.default_pool
    }

    /// Borrow the original config.
    #[must_use]
    pub fn config(&self) -> &PgConfig {
        &self.cfg
    }
}

/// Build one `deadpool` pool for a given DSN, applying the timeouts +
/// hooks from `cfg`. The hooks are wired identically across every pool so
/// idle-recycle, statement timeout, and the rollback-on-recycle invariant
/// hold for both the source-scope DSN and per-binding overrides.
fn build_pool(dsn: &str, cfg: &PgConfig) -> Result<Pool, SourceError> {
    let pg_cfg = tokio_postgres::Config::from_str(dsn).map_err(|e| SourceError::backend("dsn", e))?;

    let mgr_cfg = deadpool_postgres::ManagerConfig::default();
    let mgr = if pg_cfg.get_ssl_mode() == tokio_postgres::config::SslMode::Disable {
        deadpool_postgres::Manager::from_config(pg_cfg, NoTls, mgr_cfg)
    } else {
        // install_default returns Err only when a provider is already
        // installed in this process - benign when the host wired one up
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
    // pre_recycle fires before every checkout from the pool. an explicit
    // ROLLBACK guarantees no inherited transaction state, so a session
    // that returned its connection without committing/rolling back
    // (panic, drop, future cancellation) cannot leak its snapshot to the
    // next checkout. ROLLBACK outside a txn is a postgres NOTICE, not an
    // error, so this stays idempotent.
    builder = builder.pre_recycle(Hook::async_fn(|client, _| {
        Box::pin(async move {
            client
                .batch_execute("ROLLBACK")
                .await
                .map_err(|e| HookError::message(format!("pre_recycle rollback: {e}")))
        })
    }));
    builder.build().map_err(|e| SourceError::backend("pool create", e))
}

#[async_trait]
impl Source for PgSource {
    async fn stream_rows<'a>(
        &'a self,
        binding: &'a SourceBinding,
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        fetch::stream_rows(self.pool_for(binding)?, binding.clone()).await
    }

    async fn stream_rows_by_id<'a>(
        &'a self,
        binding: &'a SourceBinding,
        ids: &'a [i64],
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        fetch::stream_rows_by_id(self.pool_for(binding)?, binding.clone(), ids.to_vec()).await
    }

    async fn stream_feature_ids<'a>(
        &'a self,
        binding: &'a SourceBinding,
    ) -> Result<BoxStream<'a, Result<i64, SourceError>>, SourceError> {
        fetch::stream_feature_ids(self.pool_for(binding)?, binding.clone()).await
    }

    async fn open_compile_session<'a>(
        &'a self,
        binding: &'a SourceBinding,
    ) -> Result<Box<dyn CompileSession + 'a>, SourceError> {
        let session = compile_session::PgCompileSession::open(self.pool_for(binding)?, binding.clone()).await?;
        Ok(Box::new(session))
    }

    /// Probe `pg_publication_tables` for the wired publication and
    /// classify each requested collection as `Healthy` or `Unpublished`.
    /// Backstop for the "binding silently dropped from the publication"
    /// case the in-band Relation messages cannot deliver.
    async fn probe_binding_health(
        &self,
        collections: &[SourceCollectionId],
    ) -> Result<Vec<BindingHealth>, SourceError> {
        if collections.is_empty() {
            return Ok(Vec::new());
        }
        let topology = self.topology.as_ref().ok_or_else(|| {
            SourceError::InvalidBinding("ReplicationTopology not wired; call PgSource::with_topology".into())
        })?;
        // probe runs against the source-scope DSN; the publication that owns
        // logical replication lives on that connection regardless of any
        // per-binding override.
        let client = self.pool().get().await.map_err(|e| SourceError::backend("pool", e))?;
        let rows = client
            .query(
                "select schemaname, tablename \
                   from pg_publication_tables \
                  where pubname = $1",
                &[&self.cfg.publication],
            )
            .await
            .map_err(|e| SourceError::backend("pg_publication_tables", e))?;
        let published: std::collections::HashSet<(String, String)> = rows
            .into_iter()
            .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
            .collect();
        Ok(collections
            .iter()
            .map(
                |collection| match topology.collections.iter().find(|c| &c.collection == collection) {
                    Some(top) if published.contains(&(top.schema.clone(), top.table.clone())) => {
                        BindingHealth::Healthy(collection.clone())
                    }
                    // topology mismatch is treated as unpublished: a binding
                    // not in the configured topology cannot be routed even if
                    // the table happens to exist in the publication.
                    Some(_) => BindingHealth::Unpublished(collection.clone()),
                    None => BindingHealth::Unpublished(collection.clone()),
                },
            )
            .collect())
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
    /// SQL NULL - currently produced only by callers that explicitly bind it.
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

// page-keyed e2e coverage lives in `crates/bin/mars` under the `e2e` feature.

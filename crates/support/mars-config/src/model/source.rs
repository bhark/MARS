use std::time::Duration;

use mars_types::CrsCode;
use serde::{Deserialize, Serialize};

use crate::ConfigError;
use crate::units;

/// Source database configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    /// Source kind discriminator (e.g. `postgis`).
    #[serde(rename = "type")]
    pub kind: String,
    /// Database connection string.
    pub dsn: String,
    /// Native CRS reported by the source.
    pub native_crs: CrsCode,
    /// Optional change-feed configuration.
    #[serde(default)]
    pub change_feed: Option<ChangeFeed>,
    /// Connection-pool tuning. Defaults are conservative and adapter-specific.
    #[serde(default)]
    pub pool: SourcePool,
    /// Optional catalog-provisioning surface consumed by `mars setup` /
    /// `mars teardown`. When set, names and schemas declared here are the
    /// single source of truth across deployment shapes (CLI, operator-driven
    /// bootstrap Job, manual SQL).
    #[serde(default)]
    pub bootstrap: Option<Bootstrap>,
}

/// Connection-pool tuning surface for source adapters. All fields are optional;
/// adapters fall back to library defaults when a value is not set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourcePool {
    /// Maximum number of connections held by the pool.
    #[serde(default)]
    pub max_size: Option<usize>,
    /// Recycle (idle) timeout in seconds; an idle connection past this age is
    /// discarded on next checkout.
    #[serde(default)]
    pub recycle_timeout_secs: Option<u64>,
    /// Per-statement timeout in milliseconds; applied via `SET statement_timeout`
    /// on every checkout.
    #[serde(default)]
    pub statement_timeout_ms: Option<u64>,
    /// Bound on concurrent in-flight queries pipelined on a single connection
    /// when fetching a batch of cells. Adapters apply a small default when unset.
    #[serde(default)]
    pub fetch_concurrency: Option<usize>,
    /// Replication-only: max time the worker will wait for the consumer to
    /// accept a committed batch. Stalls past this budget abort the subscription
    /// so the upstream slot does not pin pg WAL indefinitely.
    #[serde(default)]
    pub batch_send_timeout_secs: Option<u64>,
}

/// Change-feed configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeFeed {
    /// Change-feed kind (e.g. `pgoutput`, `polling`).
    #[serde(rename = "type")]
    pub kind: String,
    /// Logical replication publication name.
    #[serde(default)]
    pub publication: Option<String>,
    /// Logical replication slot name.
    #[serde(default)]
    pub slot: Option<String>,
    /// Polling interval for the polling fallback.
    #[serde(default)]
    pub poll_interval: Option<String>,
}

impl ChangeFeed {
    /// Resolve `poll_interval` to a `Duration` if set.
    pub fn poll_interval_dur(&self) -> Result<Option<Duration>, ConfigError> {
        self.poll_interval.as_deref().map(units::parse_duration).transpose()
    }
}

/// Catalog-provisioning surface. Names and schemas listed here are exactly
/// what `mars setup` will CREATE and what `mars teardown` will DROP. The
/// publication and slot names themselves live on [`ChangeFeed`] so the
/// subscriber and bootstrap cannot drift.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bootstrap {
    /// Login role to provision with REPLICATION + SELECT on the listed schemas.
    pub role: String,
    /// Schemas whose tables are published. Must be non-empty.
    pub schemas: Vec<String>,
}

use std::time::Duration;

use mars_types::CrsCode;
use serde::{Deserialize, Serialize};

use crate::ConfigError;
use crate::SourceId;
use crate::units;

/// Default id used when YAML omits one. Lets single-source configs (legacy
/// or new) reference the source from bindings without naming it. See also
/// [`super::layer::default_binding_source_id`].
pub const DEFAULT_SOURCE_ID: &str = "default";

fn default_source_id() -> SourceId {
    SourceId::new(DEFAULT_SOURCE_ID)
}

/// One configured data source. Multiple sources can coexist in the same
/// service (e.g. a postgis source alongside a vectorfile source) and each
/// layer binding picks which one feeds it via [`super::layer::SourceBinding::source`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    /// Stable identifier referenced by per-layer bindings. Must be unique
    /// across the `sources:` list. Defaults to [`DEFAULT_SOURCE_ID`] when
    /// the YAML omits it (single-source configs).
    #[serde(default = "default_source_id")]
    pub id: SourceId,
    /// CRS the source delivers to the compiler. For file-based sources whose
    /// on-disk CRS differs from this, the adapter is expected to reproject
    /// before emitting WKB.
    pub native_crs: CrsCode,
    /// Backend-specific configuration. Wire form is internally tagged on
    /// `type:` — `type: postgis` for a postgres source, `type: vectorfile`
    /// for an object-store-backed file source.
    #[serde(flatten)]
    pub backend: SourceBackend,
}

/// Tagged enum of source backends. Add a variant + an adapter when adding a
/// new backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceBackend {
    /// PostGIS-backed source. Logical-replication aware.
    Postgis(PostgisBackend),
    /// File-format vector source read from an object store (s3://, gs://,
    /// file://, https://). Each binding names a URI + format hint + source
    /// CRS; the adapter caches and reprojects to [`Source::native_crs`]
    /// before emitting WKB.
    VectorFile(VectorFileBackend),
}

/// PostGIS-backed source configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PostgisBackend {
    /// Database connection string.
    pub dsn: String,
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

/// Vector-file source configuration. The actual files are addressed per
/// binding via `uri:` / `format:` / `source_crs:`; this block carries only
/// adapter-wide settings (cache location, polling cadence).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VectorFileBackend {
    /// Local on-disk cache root. Keyed by (uri, etag); repeated compile
    /// passes reuse cached bodies. Required.
    pub cache_dir: String,
    /// Polling interval for the etag-based change feed. When set, the
    /// adapter HEADs each tracked URI on this cadence and emits a `Rebind`
    /// event when the etag changes. None disables the change feed
    /// (snapshot-only).
    #[serde(default)]
    pub poll_interval: Option<String>,
    /// Cache size cap. Unit-suffixed byte literal (e.g. `"8GiB"`). None =
    /// uncapped (cache grows until full disk).
    #[serde(default)]
    pub cache_max_size: Option<String>,
}

impl VectorFileBackend {
    /// Resolve `poll_interval` to a `Duration` if set.
    pub fn poll_interval_dur(&self) -> Result<Option<Duration>, ConfigError> {
        self.poll_interval.as_deref().map(units::parse_duration).transpose()
    }

    /// Resolve `cache_max_size` to bytes if set.
    pub fn cache_max_size_bytes(&self) -> Result<Option<u64>, ConfigError> {
        self.cache_max_size.as_deref().map(units::parse_bytes).transpose()
    }
}

impl Source {
    /// Borrow the postgis backend config if this source is a postgis source.
    #[must_use]
    pub fn postgis(&self) -> Option<&PostgisBackend> {
        match &self.backend {
            SourceBackend::Postgis(b) => Some(b),
            _ => None,
        }
    }

    /// Borrow the vector-file backend config if this source is a vectorfile.
    #[must_use]
    pub fn vectorfile(&self) -> Option<&VectorFileBackend> {
        match &self.backend {
            SourceBackend::VectorFile(b) => Some(b),
            _ => None,
        }
    }

    /// Wire kind discriminator, useful for diagnostics.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self.backend {
            SourceBackend::Postgis(_) => "postgis",
            SourceBackend::VectorFile(_) => "vectorfile",
        }
    }
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

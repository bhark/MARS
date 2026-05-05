//! MARS observability primitives.
//!
//! Stable names for metrics (so dashboards survive code refactors), the
//! tracing-subscriber bootstrap, the JSON log formatter wiring, and the
//! Prometheus [`Metrics`] facade. Per SPEC §15.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Opts, Registry, TextEncoder,
};
use tracing_subscriber::EnvFilter;

#[derive(Debug, thiserror::Error)]
pub enum ObservabilityError {
    #[error("subscriber init error")]
    Subscriber(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("metrics registry error: {0}")]
    Registry(#[from] prometheus::Error),
}

/// Stable metric names. Use constants so dashboards survive refactors.
pub mod metrics {
    pub const REQUEST_TOTAL: &str = "mars_request_total";
    pub const REQUEST_DURATION: &str = "mars_request_duration_seconds";
    pub const REQUEST_FEATURES_READ: &str = "mars_request_features_read";
    pub const REQUEST_FEATURES_DRAWN: &str = "mars_request_features_drawn";
    pub const REQUEST_BYTES_READ: &str = "mars_request_bytes_read";

    pub const ARTIFACT_LOOKUP_SECONDS: &str = "mars_artifact_lookup_seconds";
    pub const ARTIFACT_READ_SECONDS: &str = "mars_artifact_read_seconds";
    pub const DECODE_SECONDS: &str = "mars_decode_seconds";
    pub const STYLE_SECONDS: &str = "mars_style_seconds";
    pub const LABEL_SECONDS: &str = "mars_label_seconds";
    pub const RENDER_SECONDS: &str = "mars_render_seconds";
    pub const ENCODE_SECONDS: &str = "mars_encode_seconds";
    pub const REPROJECT_SECONDS: &str = "mars_reproject_seconds";

    pub const CACHE_HITS: &str = "mars_cache_hits_total";
    pub const CACHE_MISSES: &str = "mars_cache_misses_total";
    pub const CACHE_BYTES: &str = "mars_cache_bytes";

    pub const COMPILER_CHANGE_EVENTS: &str = "mars_compiler_change_events_total";
    pub const COMPILER_DIRTY_CELLS: &str = "mars_compiler_dirty_cells_total";
    pub const COMPILER_REBUILD_DURATION: &str = "mars_compiler_rebuild_duration_seconds";
    pub const COMPILER_WINDOW_LAG: &str = "mars_compiler_window_lag_seconds";
    pub const MANIFEST_VERSION: &str = "mars_manifest_version";
    pub const MANIFEST_REJECT_TOTAL: &str = "mars_manifest_reject_total";
    pub const ARTIFACT_VERSION_IN_USE: &str = "mars_artifact_version_in_use";
}

/// Initialise the global tracing subscriber.
///
/// `log_level` is read from `config.observability.log_level`; when present it
/// takes precedence over `RUST_LOG`. When both are absent the default is
/// `info`. JSON output when `json` is true (matches
/// `observability.log_format: json` in SPEC §5.2).
pub fn init_tracing(json: bool, log_level: Option<&str>) -> Result<(), ObservabilityError> {
    let filter = if let Some(level) = log_level {
        EnvFilter::new(level)
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    };
    let result = if json {
        tracing_subscriber::fmt().with_env_filter(filter).json().try_init()
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).try_init()
    };
    result.map_err(ObservabilityError::Subscriber)
}

/// Bounded label values for `mars_manifest_reject_total{reason}`. Keeps
/// cardinality flat regardless of the underlying error string.
pub mod reject_reason {
    pub const BACKWARDS_VERSION: &str = "backwards_version";
    pub const UNSUPPORTED_FORMAT_VERSION: &str = "unsupported_format_version";
    pub const PARSE_ERROR: &str = "parse_error";
    pub const INVALID_SNAPSHOT: &str = "invalid_snapshot";
    pub const VALIDATION_ERROR: &str = "validation_error";
}

/// Strongly-typed Prometheus metrics facade. Cheap to clone (`Arc` inside).
///
/// Hides the underlying `Registry` and individual metric handles; callers can
/// only emit through the typed methods, which keeps label cardinality bounded.
#[derive(Clone)]
pub struct Metrics {
    inner: Arc<MetricsInner>,
}

struct MetricsInner {
    registry: Registry,
    request_total: IntCounterVec,
    request_duration: HistogramVec,
    manifest_version: IntGauge,
    manifest_reject_total: IntCounterVec,
}

impl std::fmt::Debug for Metrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Metrics").finish_non_exhaustive()
    }
}

impl Metrics {
    /// Build a new metrics facade with a fresh registry.
    pub fn new() -> Result<Self, ObservabilityError> {
        let registry = Registry::new();

        let request_total = IntCounterVec::new(
            Opts::new(metrics::REQUEST_TOTAL, "total HTTP requests"),
            &["interface", "status"],
        )?;
        // buckets cover sub-millisecond render to multi-second slow paths.
        let request_duration = HistogramVec::new(
            HistogramOpts::new(metrics::REQUEST_DURATION, "HTTP request duration in seconds")
                .buckets(vec![
                    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
                ]),
            &["interface"],
        )?;
        let manifest_version =
            IntGauge::new(metrics::MANIFEST_VERSION, "active manifest version")?;
        let manifest_reject_total = IntCounterVec::new(
            Opts::new(
                metrics::MANIFEST_REJECT_TOTAL,
                "total manifest snapshots rejected by the runtime",
            ),
            &["reason"],
        )?;

        registry.register(Box::new(request_total.clone()))?;
        registry.register(Box::new(request_duration.clone()))?;
        registry.register(Box::new(manifest_version.clone()))?;
        registry.register(Box::new(manifest_reject_total.clone()))?;

        Ok(Self {
            inner: Arc::new(MetricsInner {
                registry,
                request_total,
                request_duration,
                manifest_version,
                manifest_reject_total,
            }),
        })
    }

    /// Record one completed HTTP request.
    pub fn observe_request(&self, interface: &str, status: u16, duration: Duration) {
        let status_str = status.to_string();
        self.inner
            .request_total
            .with_label_values(&[interface, &status_str])
            .inc();
        self.inner
            .request_duration
            .with_label_values(&[interface])
            .observe(duration.as_secs_f64());
    }

    /// Set the active manifest version gauge.
    pub fn set_manifest_version(&self, version: u64) {
        // gauge is i64 internally; manifest versions are monotonic and small in
        // practice, but saturate on overflow rather than wrap.
        let v = i64::try_from(version).unwrap_or(i64::MAX);
        self.inner.manifest_version.set(v);
    }

    /// Increment the manifest reject counter for `reason`. Use the constants
    /// in [`reject_reason`] so labels stay bounded.
    pub fn inc_manifest_reject(&self, reason: &str) {
        self.inner
            .manifest_reject_total
            .with_label_values(&[reason])
            .inc();
    }

    /// Encode the current registry as Prometheus text exposition format.
    pub fn encode_text(&self) -> Result<String, ObservabilityError> {
        let encoder = TextEncoder::new();
        let mut buf = Vec::new();
        encoder.encode(&self.inner.registry.gather(), &mut buf)?;
        String::from_utf8(buf).map_err(|e| ObservabilityError::Registry(prometheus::Error::Msg(e.to_string())))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn metrics_round_trip() {
        let m = Metrics::new().unwrap();
        m.observe_request("wms", 200, Duration::from_millis(12));
        m.set_manifest_version(42);
        m.inc_manifest_reject(reject_reason::BACKWARDS_VERSION);
        let text = m.encode_text().unwrap();
        assert!(text.contains("mars_request_total"));
        assert!(text.contains("interface=\"wms\""));
        assert!(text.contains("status=\"200\""));
        assert!(text.contains("mars_request_duration_seconds"));
        assert!(text.contains("mars_manifest_version 42"));
        assert!(text.contains("mars_manifest_reject_total"));
        assert!(text.contains("reason=\"backwards_version\""));
    }
}

//! MARS observability primitives.
//!
//! Stable names for metrics (so dashboards survive code refactors), the
//! tracing-subscriber bootstrap, the JSON log formatter wiring, and the
//! Prometheus [`Metrics`] facade.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use prometheus::{
    Encoder, Gauge, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts, Registry,
    TextEncoder,
};
use tracing_subscriber::EnvFilter;

#[derive(Debug, thiserror::Error)]
pub enum ObservabilityError {
    #[error("subscriber init error")]
    Subscriber(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("metrics registry error: {0}")]
    Registry(#[from] prometheus::Error),
}

/// Stable metric names for the metrics actually exposed by [`Metrics`]. Names
/// land in dashboards, so the public surface here mirrors the facade exactly;
/// only metrics with a typed setter belong in this module.
pub mod metrics {
    pub const REQUEST_TOTAL: &str = "mars_request_total";
    pub const REQUEST_DURATION: &str = "mars_request_duration_seconds";

    pub const LABEL_SECONDS: &str = "mars_label_seconds";

    pub const COMPILER_CHANGE_EVENTS: &str = "mars_compiler_change_events_total";
    pub const COMPILER_DIRTY_CELLS: &str = "mars_compiler_dirty_cells_total";
    pub const COMPILER_REBUILD_DURATION: &str = "mars_compiler_rebuild_duration_seconds";
    pub const COMPILER_WINDOW_LAG: &str = "mars_compiler_window_lag_seconds";
    pub const COMPILER_PUBLISH_RETRIES: &str = "mars_compiler_publish_retries_total";
    pub const COMPILER_SIDECAR_THRESHOLD_WARNINGS: &str = "mars_compiler_sidecar_threshold_warnings_total";
    /// Counter labelled by `outcome` ("ok" | "error") tracking opportunistic
    /// rebalance ticks driven by `compiler.rebalance.window`.
    pub const COMPILER_REBALANCE_RUNS: &str = "mars_compiler_rebalance_runs_total";
    pub const CAPABILITIES_REBUILD_FAILURES: &str = "mars_capabilities_rebuild_failures_total";
    pub const MANIFEST_VERSION: &str = "mars_manifest_version";
    pub const MANIFEST_REJECT_TOTAL: &str = "mars_manifest_reject_total";

    /// Counter labelled by `layer` tracking features whose class chain failed
    /// to resolve a stylesheet entry; the runtime drops them rather than
    /// painting a diagnostic fallback colour.
    pub const RENDER_FEATURE_UNSTYLED: &str = "mars_render_feature_unstyled_total";

    /// Counter labelled by `binding` tracking features dropped by the compiler
    /// at emit time because no layer's class chain matched. these features
    /// would otherwise bloat the geometry payload and increment
    /// `mars_render_feature_unstyled_total` on every render pass.
    pub const COMPILER_FEATURES_UNMATCHED: &str = "mars_compiler_features_unmatched_total";

    /// Counter labelled by `adapter` (e.g. "postgres", "store_fs", "store_s3",
    /// "render") and `kind` (a stable, short, adapter-defined label - typically
    /// the `what` field of the adapter's `Backend` error variant).
    pub const ADAPTER_ERROR_TOTAL: &str = "mars_adapter_error_total";
}

/// Bounded label values for `mars_adapter_error_total{adapter}`. Adapter
/// crates should reference these constants rather than free-form strings so
/// label cardinality stays bounded.
pub mod adapter {
    pub const POSTGRES: &str = "postgres";
    pub const STORE_FS: &str = "store_fs";
    pub const STORE_S3: &str = "store_s3";
    pub const RENDER: &str = "render";
}

/// Initialise the global tracing subscriber.
///
/// `log_level` is read from `config.observability.log_level`; when present it
/// takes precedence over `RUST_LOG`. When both are absent the default is
/// `info`. JSON output when `json` is true (matches
/// `observability.log_format: json`).
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
    pub const HASH_MISMATCH: &str = "hash_mismatch";
    pub const IO_ERROR: &str = "io_error";
}

/// Bounded label values for `mars_compiler_rebalance_runs_total{outcome}`.
pub mod rebalance_outcome {
    pub const OK: &str = "ok";
    pub const ERROR: &str = "error";
}

/// Bucket an HTTP status into one of `2xx/3xx/4xx/5xx/other`. We deliberately
/// drop the exact code from the metric label to keep cardinality bounded:
/// emitting raw status codes multiplies by interface (and any future label),
/// which is the canonical Prometheus footgun. Specific codes belong in logs
/// and traces, not in metric labels.
pub mod status_class {
    pub const C2XX: &str = "2xx";
    pub const C3XX: &str = "3xx";
    pub const C4XX: &str = "4xx";
    pub const C5XX: &str = "5xx";
    pub const OTHER: &str = "other";

    pub fn bucket(status: u16) -> &'static str {
        match status {
            200..=299 => C2XX,
            300..=399 => C3XX,
            400..=499 => C4XX,
            500..=599 => C5XX,
            _ => OTHER,
        }
    }
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
    compiler_change_events: IntCounter,
    compiler_dirty_cells: IntCounter,
    compiler_rebuild_duration: Histogram,
    compiler_window_lag: Gauge,
    compiler_publish_retries: IntCounter,
    compiler_sidecar_threshold_warnings: IntCounterVec,
    compiler_rebalance_runs: IntCounterVec,
    capabilities_rebuild_failures: IntCounter,
    label_seconds: Histogram,
    render_feature_unstyled: IntCounterVec,
    compiler_features_unmatched: IntCounterVec,
    adapter_error_total: IntCounterVec,
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
            HistogramOpts::new(metrics::REQUEST_DURATION, "HTTP request duration in seconds").buckets(vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ]),
            &["interface"],
        )?;
        let manifest_version = IntGauge::new(metrics::MANIFEST_VERSION, "active manifest version")?;
        let manifest_reject_total = IntCounterVec::new(
            Opts::new(
                metrics::MANIFEST_REJECT_TOTAL,
                "total manifest snapshots rejected by the runtime",
            ),
            &["reason"],
        )?;
        let compiler_change_events = IntCounter::new(
            metrics::COMPILER_CHANGE_EVENTS,
            "total compiler change events processed",
        )?;
        let compiler_dirty_cells = IntCounter::new(
            metrics::COMPILER_DIRTY_CELLS,
            "total cells marked dirty by the compiler",
        )?;
        let compiler_rebuild_duration = Histogram::with_opts(
            HistogramOpts::new(
                metrics::COMPILER_REBUILD_DURATION,
                "compiler rebuild duration in seconds",
            )
            .buckets(vec![0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0]),
        )?;
        let compiler_window_lag = Gauge::new(
            metrics::COMPILER_WINDOW_LAG,
            "compiler change feed window lag in seconds",
        )?;
        let compiler_publish_retries = IntCounter::new(
            metrics::COMPILER_PUBLISH_RETRIES,
            "total compiler publish retries on transient store errors",
        )?;
        let compiler_sidecar_threshold_warnings = IntCounterVec::new(
            Opts::new(
                metrics::COMPILER_SIDECAR_THRESHOLD_WARNINGS,
                "total page-membership sidecar size threshold warnings, labeled by binding",
            ),
            &["binding"],
        )?;
        let compiler_rebalance_runs = IntCounterVec::new(
            Opts::new(
                metrics::COMPILER_REBALANCE_RUNS,
                "total opportunistic rebalance ticks, labeled by outcome",
            ),
            &["outcome"],
        )?;
        let capabilities_rebuild_failures = IntCounter::new(
            metrics::CAPABILITIES_REBUILD_FAILURES,
            "total failures rebuilding the cached WMS capabilities document",
        )?;
        let label_seconds = Histogram::with_opts(
            HistogramOpts::new(metrics::LABEL_SECONDS, "label collision pass duration in seconds").buckets(vec![
                0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0,
            ]),
        )?;
        let render_feature_unstyled = IntCounterVec::new(
            Opts::new(
                metrics::RENDER_FEATURE_UNSTYLED,
                "total features dropped at render time because their class chain resolved to no stylesheet entry",
            ),
            &["layer"],
        )?;
        let compiler_features_unmatched = IntCounterVec::new(
            Opts::new(
                metrics::COMPILER_FEATURES_UNMATCHED,
                "total features dropped at compile time because no layer's class chain matched",
            ),
            &["binding"],
        )?;
        let adapter_error_total = IntCounterVec::new(
            Opts::new(
                metrics::ADAPTER_ERROR_TOTAL,
                "total errors surfaced by adapter Backend variants, labelled by adapter and kind",
            ),
            &["adapter", "kind"],
        )?;

        registry.register(Box::new(request_total.clone()))?;
        registry.register(Box::new(request_duration.clone()))?;
        registry.register(Box::new(manifest_version.clone()))?;
        registry.register(Box::new(manifest_reject_total.clone()))?;
        registry.register(Box::new(compiler_change_events.clone()))?;
        registry.register(Box::new(compiler_dirty_cells.clone()))?;
        registry.register(Box::new(compiler_rebuild_duration.clone()))?;
        registry.register(Box::new(compiler_window_lag.clone()))?;
        registry.register(Box::new(compiler_publish_retries.clone()))?;
        registry.register(Box::new(compiler_sidecar_threshold_warnings.clone()))?;
        registry.register(Box::new(compiler_rebalance_runs.clone()))?;
        registry.register(Box::new(capabilities_rebuild_failures.clone()))?;
        registry.register(Box::new(label_seconds.clone()))?;
        registry.register(Box::new(render_feature_unstyled.clone()))?;
        registry.register(Box::new(compiler_features_unmatched.clone()))?;
        registry.register(Box::new(adapter_error_total.clone()))?;

        Ok(Self {
            inner: Arc::new(MetricsInner {
                registry,
                request_total,
                request_duration,
                manifest_version,
                manifest_reject_total,
                compiler_change_events,
                compiler_dirty_cells,
                compiler_rebuild_duration,
                compiler_window_lag,
                compiler_publish_retries,
                compiler_sidecar_threshold_warnings,
                compiler_rebalance_runs,
                capabilities_rebuild_failures,
                label_seconds,
                render_feature_unstyled,
                compiler_features_unmatched,
                adapter_error_total,
            }),
        })
    }

    /// Record one completed HTTP request.
    pub fn observe_request(&self, interface: &str, status: u16, duration: Duration) {
        self.inner
            .request_total
            .with_label_values(&[interface, status_class::bucket(status)])
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
        self.inner.manifest_reject_total.with_label_values(&[reason]).inc();
    }

    /// Increment the compiler change-event counter.
    pub fn inc_compiler_change_events(&self) {
        self.inner.compiler_change_events.inc();
    }

    /// Increment the compiler dirty-cell counter by `n`.
    pub fn inc_compiler_dirty_cells(&self, n: u64) {
        self.inner.compiler_dirty_cells.inc_by(n);
    }

    /// Record one compiler rebuild duration.
    pub fn observe_compiler_rebuild_duration(&self, duration: Duration) {
        self.inner.compiler_rebuild_duration.observe(duration.as_secs_f64());
    }

    /// Set the compiler change-feed window lag gauge.
    pub fn set_compiler_window_lag(&self, duration: Duration) {
        self.inner.compiler_window_lag.set(duration.as_secs_f64());
    }

    /// Increment the compiler publish-retry counter.
    pub fn inc_compiler_publish_retries(&self) {
        self.inner.compiler_publish_retries.inc();
    }

    /// Increment the page-membership sidecar threshold-warning counter for
    /// `binding`. Operators see this metric when the encoded sidecar
    /// crosses the configured size warning threshold.
    pub fn inc_compiler_sidecar_threshold_warning(&self, binding: &str) {
        self.inner
            .compiler_sidecar_threshold_warnings
            .with_label_values(&[binding])
            .inc();
    }

    /// Increment the opportunistic-rebalance run counter. `outcome` is
    /// `"ok"` on success (including no-op publishes) and `"error"` when the
    /// run returns an error. Use the constants in
    /// [`rebalance_outcome`] to keep label cardinality bounded.
    pub fn inc_compiler_rebalance_run(&self, outcome: &str) {
        self.inner.compiler_rebalance_runs.with_label_values(&[outcome]).inc();
    }

    /// Increment the capabilities rebuild failure counter.
    pub fn inc_capabilities_rebuild_failures(&self) {
        self.inner.capabilities_rebuild_failures.inc();
    }

    /// Record one label collision-pass duration.
    pub fn observe_label_seconds(&self, duration: Duration) {
        self.inner.label_seconds.observe(duration.as_secs_f64());
    }

    /// Increment the unstyled-feature counter for `layer`. Called when a
    /// feature's class chain resolves to no stylesheet entry; the runtime
    /// drops the feature rather than painting a diagnostic colour.
    pub fn inc_render_feature_unstyled(&self, layer: &str, n: u64) {
        self.inner.render_feature_unstyled.with_label_values(&[layer]).inc_by(n);
    }

    /// Increment the compiler unmatched-feature counter for `binding`. Called
    /// when a row is dropped at emit time because no layer's class chain
    /// matched it; keeps the geometry payload tight and avoids paying the
    /// `feature_unstyled` cost on every subsequent render.
    pub fn inc_compiler_features_unmatched(&self, binding: &str, n: u64) {
        self.inner
            .compiler_features_unmatched
            .with_label_values(&[binding])
            .inc_by(n);
    }

    /// Increment the adapter error counter. `adapter` is a stable short label
    /// from [`adapter`]; `kind` is a stable adapter-defined short string,
    /// typically the `what` carried by the adapter's `Backend` error variant.
    /// Both labels MUST be bounded-cardinality strings - never raw error
    /// messages or anything sourced from user input.
    pub fn inc_adapter_error(&self, adapter: &str, kind: &str) {
        self.inner.adapter_error_total.with_label_values(&[adapter, kind]).inc();
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
        m.inc_compiler_change_events();
        m.inc_compiler_dirty_cells(7);
        m.observe_compiler_rebuild_duration(Duration::from_secs_f64(1.23));
        m.set_compiler_window_lag(Duration::from_secs_f64(0.5));
        m.inc_render_feature_unstyled("Bygning", 3);
        m.inc_compiler_features_unmatched("buildings_live", 5);
        m.inc_adapter_error(adapter::POSTGRES, "fetch_cells");
        m.inc_adapter_error(adapter::STORE_FS, "open");
        let text = m.encode_text().unwrap();
        assert!(text.contains("mars_request_total"));
        assert!(text.contains("interface=\"wms\""));
        assert!(text.contains("status=\"2xx\""));
        assert!(text.contains("mars_request_duration_seconds"));
        assert!(text.contains("mars_manifest_version 42"));
        assert!(text.contains("mars_manifest_reject_total"));
        assert!(text.contains("reason=\"backwards_version\""));
        assert!(text.contains("mars_compiler_change_events_total"));
        assert!(text.contains("mars_compiler_dirty_cells_total"));
        assert!(text.contains("mars_compiler_rebuild_duration_seconds"));
        assert!(text.contains("mars_compiler_window_lag_seconds"));
        assert!(text.contains("mars_render_feature_unstyled_total"));
        assert!(text.contains("layer=\"Bygning\""));
        assert!(text.contains("mars_compiler_features_unmatched_total"));
        assert!(text.contains("binding=\"buildings_live\""));
        assert!(text.contains("mars_adapter_error_total"));
        assert!(text.contains("adapter=\"postgres\""));
        assert!(text.contains("kind=\"fetch_cells\""));
        assert!(text.contains("adapter=\"store_fs\""));
    }
}

//! MARS observability primitives.
//!
//! Stable names for metrics (so dashboards survive code refactors), the
//! tracing-subscriber bootstrap, and the JSON log formatter wiring.
//! Per SPEC §15.

#![forbid(unsafe_code)]

use tracing_subscriber::EnvFilter;

#[derive(Debug, thiserror::Error)]
pub enum ObservabilityError {
    #[error("subscriber init error")]
    Subscriber(#[source] Box<dyn std::error::Error + Send + Sync>),
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
    pub const ARTIFACT_VERSION_IN_USE: &str = "mars_artifact_version_in_use";
}

/// Initialise the global tracing subscriber. Honours `RUST_LOG`. JSON output
/// when `json` is true (matches `observability.log_format: json` in SPEC §5.2).
pub fn init_tracing(json: bool) -> Result<(), ObservabilityError> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let result = if json {
        tracing_subscriber::fmt().with_env_filter(filter).json().try_init()
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).try_init()
    };
    result.map_err(|e| ObservabilityError::Subscriber(e))
}

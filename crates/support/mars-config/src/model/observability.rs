use serde::{Deserialize, Serialize};

/// Observability settings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Observability {
    /// `info`, `debug`, ...
    #[serde(default)]
    pub log_level: Option<String>,
    /// `json` or `text`.
    #[serde(default)]
    pub log_format: Option<String>,
    /// Prometheus listen address.
    #[serde(default)]
    pub metrics_listen: Option<String>,
    /// OTLP tracing config.
    #[serde(default)]
    pub tracing: Option<TracingConfig>,
}

/// OTLP tracing configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracingConfig {
    /// Tracing kind (`otlp`).
    #[serde(rename = "type")]
    pub kind: String,
    /// OTLP collector endpoint.
    pub endpoint: String,
    /// Sample rate.
    #[serde(default)]
    pub sample_rate: Option<f64>,
}

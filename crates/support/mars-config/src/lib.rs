//! MARS service configuration.
//!
//! Owns the YAML model, the `!include` resolver, `${ENV:-default}` substitution
//! and post-parse validation. Filesystem and env access are why this lives in
//! `support/` rather than `domain/` — the model itself is data, but the loader
//! is I/O.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io error: {0}")]
    Io(String),
    #[error("yaml parse error: {0}")]
    Parse(String),
    #[error("env var not set and no default: {0}")]
    EnvMissing(String),
    #[error("validation: {0}")]
    Invalid(String),
    #[error("not implemented: {what}")]
    NotImplemented { what: &'static str },
}

/// Top-level service configuration. SPEC §5.2.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    pub service: ServiceMeta,
    // source / artifacts / scales / cells / interfaces / styles / layers
    // remain as opaque YAML in Phase 0; their typed models land alongside the
    // first real consumers in Phase 1.
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServiceMeta {
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, rename = "abstract")]
    pub abstract_: String,
    #[serde(default)]
    pub contact_email: String,
}

/// Load a configuration document from `path`, resolving `!include` directives
/// and `${ENV}` substitutions. Phase 0 stub.
pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
    let _ = path.as_ref();
    Err(ConfigError::NotImplemented {
        what: "mars-config::load",
    })
}

/// Validate a parsed configuration. Phase 0 stub.
pub fn validate(_config: &Config, _config_dir: &Path) -> Result<(), ConfigError> {
    Err(ConfigError::NotImplemented {
        what: "mars-config::validate",
    })
}

/// Helper exposing the config-file directory so includes resolve relative to it.
#[must_use]
pub fn config_dir(path: &Path) -> PathBuf {
    path.parent().map(PathBuf::from).unwrap_or_default()
}

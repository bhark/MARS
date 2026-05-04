//! MARS service configuration.
//!
//! Owns the YAML model, the `!include` resolver, `${ENV:-default}` substitution
//! and post-parse validation. Filesystem and env access are why this lives in
//! `support/` rather than `domain/` - the model itself is data, but the loader
//! is I/O.
//!
//! Pipeline (see [`load`]):
//! 1. read file -> string
//! 2. apply env substitution to the source string
//! 3. parse to `serde_yml::Value`
//! 4. resolve `!include` directives recursively (each included file goes
//!    through steps 1-3 itself, with per-file cycle detection)
//! 5. deserialise into the typed [`Config`]
//!
//! [`validate`] then performs cross-cutting checks that exceed serde's reach:
//! style refs resolve, source bindings reference known scale bands, and
//! every class `when:` string parses via [`mars_expr::parse`].

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

mod env_subst;
mod include;
pub mod model;
pub mod units;

pub use model::*;

/// Errors emitted by the configuration loader and validator.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Filesystem error (read, canonicalize).
    #[error("io error: {context}")]
    Io {
        /// what operation failed.
        context: String,
        /// underlying std::io error.
        #[source]
        source: std::io::Error,
    },
    /// YAML parse error.
    #[error("yaml parse error in {path}")]
    Parse {
        /// file being parsed.
        path: String,
        /// underlying serde_yml error.
        #[source]
        source: serde_yml::Error,
    },
    /// Required env var was unset and had no default.
    #[error("env var not set and no default: {0}")]
    EnvMissing(String),
    /// Post-parse validation failure.
    #[error("validation: {0}")]
    Invalid(String),
    /// Reserved for stub call sites in other crates that haven't migrated yet.
    #[error("not implemented: {what}")]
    NotImplemented {
        /// Human-readable name of the unimplemented surface.
        what: &'static str,
    },
}

/// Load a configuration document from `path`, resolving `!include` directives
/// and `${ENV}` substitutions.
pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
    let path = path.as_ref();
    let root = config_dir(path);
    let value = include::load_with_includes(path, &root)?;
    let config: Config = serde_yml::from_value(value).map_err(|e| ConfigError::Parse {
        path: path.display().to_string(),
        source: e,
    })?;
    Ok(config)
}

/// Validate a parsed configuration. Cross-cutting checks beyond serde:
/// - every layer's `style: { ref: ... }` resolves against `styles`;
/// - every source binding's `band` (when set) exists in `scales.bands`;
/// - every `cells.size_per_band` key matches a declared band;
/// - every class `when:` parses via [`mars_expr::parse`].
///
/// `config_dir` is currently unused at validate time but accepted for symmetry
/// and future-proofing - validation may grow filesystem checks (e.g. cache
/// path writability) that require it.
pub fn validate(config: &Config, config_dir: &Path) -> Result<(), ConfigError> {
    let _ = config_dir;

    let band_names: std::collections::BTreeSet<&str> = config.scales.bands.iter().map(|b| b.name.as_str()).collect();

    for k in config.cells.size_per_band.keys() {
        if !band_names.contains(k.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "cells.size_per_band references unknown band {k:?}"
            )));
        }
    }

    for layer in &config.layers {
        for (i, binding) in layer.sources.iter().enumerate() {
            if let Some(band) = &binding.band
                && !band_names.contains(band.as_str())
            {
                return Err(ConfigError::Invalid(format!(
                    "layer {} source[{i}] band {band:?} not declared in scales.bands",
                    layer.name
                )));
            }
        }

        for class in &layer.classes {
            match &class.style {
                ClassStyle::Ref { ref_ } => {
                    if !config.styles.contains_key(ref_) {
                        return Err(ConfigError::Invalid(format!(
                            "layer {} class {:?} references unknown style {:?}",
                            layer.name, class.name, ref_
                        )));
                    }
                }
                ClassStyle::Inline(_) => {}
            }

            if let Some(when) = &class.when {
                match mars_expr::parse(when) {
                    Ok(_) => {}
                    // tighten once Slice B lands: today the parser returns
                    // NotImplemented for any non-empty input. tolerate it so
                    // configs validate end-to-end while the parser is in
                    // flight.
                    Err(mars_expr::ExprError::NotImplemented { .. }) => {}
                    Err(e) => {
                        return Err(ConfigError::Invalid(format!(
                            "layer {} class {:?} when: parse error: {e}",
                            layer.name, class.name
                        )));
                    }
                }
            }
        }

        if let Some(label) = &layer.label
            && let LabelStyleAttach::Ref { ref_ } = &label.style
            && !matches!(config.styles.get(ref_), Some(StyleEntry::Label(_)))
        {
            return Err(ConfigError::Invalid(format!(
                "layer {} label references unknown or non-label style {:?}",
                layer.name, ref_
            )));
        }
    }

    Ok(())
}

/// Helper exposing the config-file directory so includes resolve relative to it.
#[must_use]
pub fn config_dir(path: &Path) -> PathBuf {
    path.parent().map(PathBuf::from).unwrap_or_default()
}

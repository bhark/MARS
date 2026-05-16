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
//! 3. parse to `serde_yaml_ng::Value`
//! 4. resolve `!include` directives recursively (each included file goes
//!    through steps 1-3 itself, with per-file cycle detection)
//! 5. deserialise into the typed [`Config`]
//!
//! [`validate`] then performs cross-cutting checks that exceed serde's reach:
//! style refs resolve, source bindings reference known scale bands, and
//! every class `when:` string parses via [`mars_expr::parse`].

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

pub mod cgroup;
mod env_subst;
mod include;
pub mod model;
pub mod units;

pub use model::*;
// re-export style value types operators reach for through mars-config so
// downstream crates (mars-compiler, bin/mars) need not depend on mars-style
// just to name a `Layer.label_survival` value.
pub use mars_style::{LabelStyle, LabelSurvival, Placement, PolygonStrategy};

mars_types::impl_string_newtype!(
    /// Scale-band identifier used in binding configuration. Binding configs
    /// name a `ScaleBand` to declare which decimation level applies for which
    /// scale window. WMTS TMS uses its own `mars_grid::BandName` instead.
    pub ScaleBand
);

mars_types::impl_string_newtype!(
    /// Stable identifier for a configured source. Each entry in `Config.sources`
    /// carries a unique `id`; per-layer bindings reference it to route to the
    /// right backend.
    pub SourceId
);

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
        /// underlying serde_yaml_ng error.
        #[source]
        source: serde_yaml_ng::Error,
    },
    /// Required env var was unset and had no default.
    #[error("env var not set and no default: {0}")]
    EnvMissing(String),
    /// Post-parse validation failure.
    #[error("validation: {0}")]
    Invalid(String),
    /// PROJ introspection failed; cannot determine whether a CRS is metric.
    /// Distinct from [`ConfigError::Invalid`] so operators can tell a "wrong
    /// CRS" mistake apart from a broken PROJ install / missing proj.db.
    #[error("proj unavailable for CRS {code:?}")]
    ProjUnavailable {
        /// CRS that PROJ failed to introspect.
        code: String,
        /// underlying mars-proj error.
        #[source]
        source: mars_proj::ProjError,
    },
}

/// Load a configuration document from `path`, resolving `!include` directives
/// and `${ENV}` substitutions.
pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
    let path = path.as_ref();
    let root = config_dir(path);
    let mut value = include::load_with_includes(path, &root)?;
    fold_legacy_source(&mut value);
    let config: Config = serde_yaml_ng::from_value(value).map_err(|e| ConfigError::Parse {
        path: path.display().to_string(),
        source: e,
    })?;
    Ok(config)
}

// wire-level back-compat: legacy YAMLs carried a singular `source: { ... }`
// block at the top level. rewrite it to plural `sources: [{ ... }]` so it
// parses against the multi-source `Config`. the folded entry's `id` field
// auto-defaults via serde, matching the per-binding default so layers don't
// need to name their source.
fn fold_legacy_source(value: &mut serde_yaml_ng::Value) {
    use serde_yaml_ng::Value;
    let Value::Mapping(m) = value else {
        return;
    };
    let has_singular = m.contains_key("source");
    let has_plural = m.contains_key("sources");
    if has_singular
        && !has_plural
        && let Some(singular) = m.remove("source")
    {
        m.insert(Value::String("sources".into()), Value::Sequence(vec![singular]));
    }
}

mod validate;

pub use validate::validate;

/// Helper exposing the config-file directory so includes resolve relative to it.
#[must_use]
pub fn config_dir(path: &Path) -> PathBuf {
    path.parent().map(PathBuf::from).unwrap_or_default()
}

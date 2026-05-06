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

    if config.service.name.trim().is_empty() {
        return Err(ConfigError::Invalid("service.name must not be empty".into()));
    }
    if config.service.name.contains(' ') {
        return Err(ConfigError::Invalid(format!(
            "service.name {:?} must not contain spaces",
            config.service.name
        )));
    }

    let crs = config.source.native_crs.as_str().trim();
    if crs.is_empty() {
        return Err(ConfigError::Invalid("source.native_crs must not be empty".into()));
    }
    if !is_metric_crs(crs) {
        return Err(ConfigError::Invalid(format!(
            "source.native_crs {:?} is not a recognised metric CRS; mars-runtime requires a metric canonical CRS \
             (units-per-metre = 1). Use a projected, metre-based EPSG code (e.g. EPSG:25832, EPSG:3857).",
            crs
        )));
    }

    let mut band_names = std::collections::BTreeSet::new();
    for band in &config.scales.bands {
        if !band_names.insert(band.name.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "duplicate band name {:?} in scales.bands",
                band.name
            )));
        }
    }

    if config.cells.size_per_band.is_empty() {
        return Err(ConfigError::Invalid("cells.size_per_band must not be empty".into()));
    }
    for k in config.cells.size_per_band.keys() {
        if !band_names.contains(k.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "cells.size_per_band references unknown band {k:?}"
            )));
        }
    }

    let mut layer_names = std::collections::BTreeSet::new();
    for layer in &config.layers {
        if !layer_names.insert(layer.name.as_str()) {
            return Err(ConfigError::Invalid(format!("duplicate layer name {:?}", layer.name)));
        }

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
                ClassStyle::Ref { name } => {
                    if !config.styles.contains_key(name) {
                        return Err(ConfigError::Invalid(format!(
                            "layer {} class {:?} references unknown style {:?}",
                            layer.name, class.name, name
                        )));
                    }
                }
                ClassStyle::Inline(_) => {}
            }

            if let Some(when) = &class.when
                && let Err(e) = mars_expr::parse(when)
            {
                return Err(ConfigError::Invalid(format!(
                    "layer {} class {:?} when: parse error: {e}",
                    layer.name, class.name
                )));
            }
        }

        if let Some(label) = &layer.label
            && let LabelStyleAttach::Ref { name } = &label.style
            && !matches!(config.styles.get(name), Some(StyleEntry::Label(_)))
        {
            return Err(ConfigError::Invalid(format!(
                "layer {} label references unknown or non-label style {:?}",
                layer.name, name
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

/// Validate that `code` is a projected (metric) CRS using PROJ introspection.
fn is_metric_crs(code: &str) -> bool {
    let crs = mars_types::CrsCode::new(code.trim());
    mars_proj::is_projected(&crs).unwrap_or(false)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::model::{Band, Cells, Layer, Scales, ServiceMeta, Source};
    use mars_types::{Bbox, CrsCode};

    fn minimal_config() -> Config {
        use crate::model::{ArtifactCache, ArtifactStore, Compiler, Interfaces, Observability, Render};
        let mut size_per_band = BTreeMap::new();
        size_per_band.insert("hi".into(), "1024m".into());
        Config {
            service: ServiceMeta {
                name: "test".into(),
                ..Default::default()
            },
            source: Source {
                kind: "memory".into(),
                dsn: "memory://".into(),
                native_crs: CrsCode::new("EPSG:25832"),
                change_feed: None,
                pool: Default::default(),
            },
            artifacts: Artifacts {
                store: ArtifactStore {
                    kind: "fs".into(),
                    endpoint: None,
                    bucket: None,
                    prefix: None,
                    path: Some("/tmp".into()),
                },
                cache: ArtifactCache {
                    path: "/tmp".into(),
                    max_size: "1GiB".into(),
                    eviction: "lru".into(),
                    trust_path_hash: false,
                },
            },
            scales: Scales {
                bands: vec![Band {
                    name: "hi".into(),
                    max_denom: 25000,
                }],
            },
            cells: Cells {
                grid: "regular".into(),
                origin: [0.0, 0.0],
                size_per_band,
                extent: Some(Bbox::new(0.0, 0.0, 1_000.0, 1_000.0)),
            },
            interfaces: Interfaces::default(),
            tile_matrix_sets: Default::default(),
            reprojection: Default::default(),
            styles: Default::default(),
            layers: vec![],
            observability: Observability::default(),
            render: Render::default(),
            compiler: Compiler::default(),
        }
    }

    #[test]
    fn rejects_empty_service_name() {
        let mut cfg = minimal_config();
        cfg.service.name = String::new();
        assert!(matches!(
            validate(&cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("service.name")
        ));
    }

    #[test]
    fn rejects_service_name_with_spaces() {
        let mut cfg = minimal_config();
        cfg.service.name = "foo bar".into();
        assert!(matches!(
            validate(&cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("spaces")
        ));
    }

    #[test]
    fn rejects_empty_native_crs() {
        let mut cfg = minimal_config();
        cfg.source.native_crs = CrsCode::new("");
        assert!(matches!(
            validate(&cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("native_crs")
        ));
    }

    #[test]
    fn rejects_duplicate_band_names() {
        let mut cfg = minimal_config();
        cfg.scales.bands.push(Band {
            name: "hi".into(),
            max_denom: 5000,
        });
        assert!(matches!(
            validate(&cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("duplicate band")
        ));
    }

    #[test]
    fn rejects_empty_size_per_band() {
        let mut cfg = minimal_config();
        cfg.cells.size_per_band.clear();
        assert!(matches!(
            validate(&cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("size_per_band")
        ));
    }

    #[test]
    fn rejects_unparseable_when_clause() {
        let mut cfg = minimal_config();
        cfg.layers = vec![Layer {
            name: mars_types::LayerId::new("roads"),
            title: String::new(),
            abstract_: String::new(),
            kind: "line".into(),
            scale: None,
            group: None,
            enable_get_feature_info: false,
            bbox: None,
            sources: vec![],
            classes: vec![crate::model::Class {
                name: "default".into(),
                title: String::new(),
                when: Some("(((".into()),
                style: ClassStyle::Inline(Default::default()),
            }],
            label: None,
        }];
        assert!(matches!(
            validate(&cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("when: parse error")
        ));
    }

    #[test]
    fn rejects_duplicate_layer_names() {
        let mut cfg = minimal_config();
        let layer = Layer {
            name: mars_types::LayerId::new("roads"),
            title: String::new(),
            abstract_: String::new(),
            kind: "line".into(),
            scale: None,
            group: None,
            enable_get_feature_info: false,
            bbox: None,
            sources: vec![],
            classes: vec![],
            label: None,
        };
        cfg.layers = vec![layer.clone(), layer];
        assert!(matches!(
            validate(&cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("duplicate layer")
        ));
    }
}

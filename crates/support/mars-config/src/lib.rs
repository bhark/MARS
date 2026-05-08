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

use mars_types::LayerId;

mod env_subst;
mod include;
pub mod model;
pub mod units;

pub use model::*;

mars_types::impl_string_newtype!(
    /// Scale-band identifier used in binding configuration (post-LAZARUS
    /// Phase B). No longer a substrate axis; binding configs name a
    /// `ScaleBand` to declare which decimation level applies for which
    /// scale window. WMTS TMS uses its own `mars_grid::BandName` instead.
    pub ScaleBand
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
    let value = include::load_with_includes(path, &root)?;
    let config: Config = serde_yaml_ng::from_value(value).map_err(|e| ConfigError::Parse {
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
    if !is_metric_crs(crs)? {
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
    // every declared band must have a cells.size_per_band entry — without one
    // the planner errors at first use, far from the config that broke it.
    for band in &config.scales.bands {
        if !config.cells.size_per_band.contains_key(band.name.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "scales.bands declares {:?} but cells.size_per_band has no entry for it",
                band.name
            )));
        }
    }

    let mut layer_names = std::collections::BTreeSet::new();
    for layer in &config.layers {
        if !layer_names.insert(layer.name.as_str()) {
            return Err(ConfigError::Invalid(format!("duplicate layer name {:?}", layer.name)));
        }

        // class names must be unique within a layer; a duplicate makes the
        // second class unreachable (first-match wins) which is almost never
        // the operator's intent.
        let mut class_names = std::collections::BTreeSet::new();
        for class in &layer.classes {
            if !class_names.insert(class.name.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "layer {} declares class {:?} more than once",
                    layer.name, class.name
                )));
            }
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

            validate_binding_from(&layer.name, i, &binding.from)?;
            validate_binding_levels(&layer.name, i, binding)?;
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

        if let Some(label) = &layer.label {
            if let LabelStyleAttach::Ref { name } = &label.style
                && !matches!(config.styles.get(name), Some(StyleEntry::Label(_)))
            {
                return Err(ConfigError::Invalid(format!(
                    "layer {} label references unknown or non-label style {:?}",
                    layer.name, name
                )));
            }

            if let Some(placement) = &label.placement {
                let geom = mars_style::LayerGeomKind::parse(layer.kind.as_str());
                let ok = match (geom, placement) {
                    (Some(mars_style::LayerGeomKind::Point), mars_style::Placement::Point) => true,
                    (Some(mars_style::LayerGeomKind::Line), mars_style::Placement::Line { .. }) => true,
                    (Some(mars_style::LayerGeomKind::Polygon), mars_style::Placement::Polygon { .. }) => true,
                    // unknown layer kind is rejected separately by other validation paths;
                    // here we only reject explicit kind/placement mismatches.
                    (None, _) => true,
                    _ => false,
                };
                if !ok {
                    return Err(ConfigError::Invalid(format!(
                        "layer {} placement does not match geometry type {:?}",
                        layer.name, layer.kind
                    )));
                }
            }
        }
    }

    Ok(())
}

/// Helper exposing the config-file directory so includes resolve relative to it.
#[must_use]
pub fn config_dir(path: &Path) -> PathBuf {
    path.parent().map(PathBuf::from).unwrap_or_default()
}

/// Reject `from:` strings that are not a single change-feed-mappable table.
/// LAZARUS §39-41 documents the v1 restriction: a binding must point at one
/// real table or a single-table view so pgoutput events map to a single
/// feature_id. Multi-table joins, embedded SELECTs, or compound DDL fragments
/// are rejected here, far from the snapshot path that would otherwise fail
/// opaquely later.
fn validate_binding_from(layer: &LayerId, idx: usize, from: &str) -> Result<(), ConfigError> {
    let trimmed = from.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] from must not be empty"
        )));
    }
    let lower = trimmed.to_ascii_lowercase();
    let bad_substr = ["(", ")", ";", " join ", " select ", " from ", " where ", " union "]
        .iter()
        .find(|s| lower.contains(*s));
    if let Some(needle) = bad_substr {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] from {from:?} is not a single-table reference \
             (contains {needle:?}); v1 bindings must name one table or a single-table view \
             so the change-feed can map events to feature ids"
        )));
    }
    // single-segment names route to `public`; allow at most `schema.table`.
    if trimmed.matches('.').count() > 1 {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] from {from:?} must be `table` or `schema.table`"
        )));
    }
    Ok(())
}

/// Validate per-level decimation config on a single binding.
fn validate_binding_levels(layer: &LayerId, idx: usize, binding: &SourceBinding) -> Result<(), ConfigError> {
    if let Some(target) = binding.page_size_target_bytes
        && target == 0
    {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] page_size_target_bytes must be > 0"
        )));
    }
    let Some(levels) = &binding.levels else {
        return Ok(());
    };
    if levels.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] levels: must not be empty when set"
        )));
    }
    let mut prev: Option<u8> = None;
    for (li, lvl) in levels.iter().enumerate() {
        if let Some(p) = prev
            && lvl.level <= p
        {
            return Err(ConfigError::Invalid(format!(
                "layer {layer} source[{idx}] levels[{li}] level {} must be strictly greater than previous {}",
                lvl.level, p
            )));
        }
        prev = Some(lvl.level);
        if !lvl.vertex_tolerance_m.is_finite() || lvl.vertex_tolerance_m < 0.0 {
            return Err(ConfigError::Invalid(format!(
                "layer {layer} source[{idx}] levels[{li}] vertex_tolerance_m must be finite and >= 0"
            )));
        }
        if !lvl.geometry_min_size_m.is_finite() || lvl.geometry_min_size_m < 0.0 {
            return Err(ConfigError::Invalid(format!(
                "layer {layer} source[{idx}] levels[{li}] geometry_min_size_m must be finite and >= 0"
            )));
        }
    }
    Ok(())
}

/// Validate that `code` is a projected (metric) CRS using PROJ introspection.
/// PROJ failures (broken install, missing proj.db) surface as
/// `ConfigError::ProjUnavailable` rather than collapsing into "not metric",
/// which would mislead operators into thinking they configured the wrong CRS.
fn is_metric_crs(code: &str) -> Result<bool, ConfigError> {
    let trimmed = code.trim();
    let crs = mars_types::CrsCode::new(trimmed);
    mars_proj::is_projected(&crs).map_err(|source| ConfigError::ProjUnavailable {
        code: trimmed.to_string(),
        source,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroUsize;

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
                    allow_http: false,
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
    fn rejects_band_without_size_per_band_entry() {
        let mut cfg = minimal_config();
        cfg.scales.bands.push(Band {
            name: "lo".into(),
            max_denom: 100_000,
        });
        // size_per_band still only knows about "hi"
        assert!(matches!(
            validate(&cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("no entry") && s.contains("lo")
        ));
    }

    #[test]
    fn rejects_duplicate_class_names_within_layer() {
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
            classes: vec![
                crate::model::Class {
                    name: "default".into(),
                    title: String::new(),
                    when: None,
                    style: ClassStyle::Inline(Default::default()),
                },
                crate::model::Class {
                    name: "default".into(),
                    title: String::new(),
                    when: None,
                    style: ClassStyle::Inline(Default::default()),
                },
            ],
            label: None,
        }];
        assert!(matches!(
            validate(&cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("more than once") && s.contains("default")
        ));
    }

    #[test]
    fn compiler_parallel_cells_yaml_roundtrip() {
        // unset → None
        let yaml = "window: 5min\n";
        let c: crate::model::Compiler = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(c.parallel_cells.is_none());

        // explicit positive value → Some(N)
        let yaml = "window: 5min\nparallel_cells: 8\n";
        let c: crate::model::Compiler = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(c.parallel_cells.map(NonZeroUsize::get), Some(8));

        // zero must be rejected at parse (NonZeroUsize)
        let yaml = "window: 5min\nparallel_cells: 0\n";
        assert!(serde_yaml_ng::from_str::<crate::model::Compiler>(yaml).is_err());
    }

    #[test]
    fn render_png_compression_yaml_roundtrip() {
        use crate::model::{PngCompression, Render};
        // unset → default (Fast)
        let yaml = "jpeg_quality: 85\npixel_budget: 256MiB\n";
        let r: Render = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(r.png_compression, PngCompression::Fast);

        // each variant deserialises from its lowercase wire form
        for (s, expected) in [
            ("none", PngCompression::None),
            ("fastest", PngCompression::Fastest),
            ("fast", PngCompression::Fast),
            ("balanced", PngCompression::Balanced),
            ("high", PngCompression::High),
        ] {
            let yaml = format!("jpeg_quality: 85\npixel_budget: 256MiB\npng_compression: {s}\n");
            let r: Render = serde_yaml_ng::from_str(&yaml).unwrap();
            assert_eq!(r.png_compression, expected, "variant {s}");
        }

        // unknown variant must be rejected
        let yaml = "jpeg_quality: 85\npixel_budget: 256MiB\npng_compression: crunchy\n";
        assert!(serde_yaml_ng::from_str::<Render>(yaml).is_err());
    }

    #[test]
    fn rejects_placement_geom_mismatch() {
        use crate::model::{LabelStyleAttach, LayerLabel};
        let mut cfg = minimal_config();
        cfg.layers = vec![Layer {
            name: mars_types::LayerId::new("roads"),
            title: String::new(),
            abstract_: String::new(),
            kind: "polygon".into(),
            scale: None,
            group: None,
            enable_get_feature_info: false,
            bbox: None,
            sources: vec![],
            classes: vec![],
            label: Some(LayerLabel {
                style: LabelStyleAttach::Inline(mars_style::LabelStyle {
                    font_family: "DejaVu Sans".into(),
                    font_size: 12.0,
                    fill: mars_style::Colour::rgb(0, 0, 0),
                    halo: None,
                    priority: 0,
                    min_distance: 0.0,
                }),
                text: "{name}".into(),
                placement: Some(mars_style::Placement::Line {
                    repeat_m: 250.0,
                    max_angle_delta_deg: 25.0,
                }),
            }),
        }];
        assert!(matches!(
            validate(&cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("placement does not match")
        ));
    }

    #[test]
    fn accepts_placement_matching_geom() {
        use crate::model::{LabelStyleAttach, LayerLabel};
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
            classes: vec![],
            label: Some(LayerLabel {
                style: LabelStyleAttach::Inline(mars_style::LabelStyle {
                    font_family: "DejaVu Sans".into(),
                    font_size: 12.0,
                    fill: mars_style::Colour::rgb(0, 0, 0),
                    halo: None,
                    priority: 0,
                    min_distance: 0.0,
                }),
                text: "{name}".into(),
                placement: Some(mars_style::Placement::Line {
                    repeat_m: 250.0,
                    max_angle_delta_deg: 25.0,
                }),
            }),
        }];
        assert!(validate(&cfg, Path::new(".")).is_ok());
    }

    fn layer_with_binding(binding: SourceBinding) -> Layer {
        Layer {
            name: mars_types::LayerId::new("roads"),
            title: String::new(),
            abstract_: String::new(),
            kind: "line".into(),
            scale: None,
            group: None,
            enable_get_feature_info: false,
            bbox: None,
            sources: vec![binding],
            classes: vec![],
            label: None,
        }
    }

    fn binding(from: &str) -> SourceBinding {
        SourceBinding {
            scale: None,
            band: None,
            from: from.into(),
            geometry_column: "geom".into(),
            id_column: Some("id".into()),
            attributes: vec![],
            levels: None,
            page_size_target_bytes: None,
        }
    }

    #[test]
    fn accepts_simple_table_binding() {
        let mut cfg = minimal_config();
        cfg.layers = vec![layer_with_binding(binding("buildings"))];
        assert!(validate(&cfg, Path::new(".")).is_ok());

        cfg.layers = vec![layer_with_binding(binding("public.buildings"))];
        assert!(validate(&cfg, Path::new(".")).is_ok());
    }

    #[test]
    fn rejects_multi_table_from() {
        for bad in [
            "(SELECT id FROM a JOIN b USING (k))",
            "a JOIN b ON a.k=b.k",
            "a; truncate b",
        ] {
            let mut cfg = minimal_config();
            cfg.layers = vec![layer_with_binding(binding(bad))];
            let err = validate(&cfg, Path::new("."));
            assert!(
                matches!(&err, Err(ConfigError::Invalid(s)) if s.contains("single-table")),
                "expected single-table rejection for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn rejects_overdotted_from() {
        let mut cfg = minimal_config();
        cfg.layers = vec![layer_with_binding(binding("a.b.c"))];
        let err = validate(&cfg, Path::new("."));
        assert!(matches!(&err, Err(ConfigError::Invalid(s)) if s.contains("schema.table")));
    }

    #[test]
    fn rejects_empty_levels() {
        let mut cfg = minimal_config();
        let mut b = binding("buildings");
        b.levels = Some(vec![]);
        cfg.layers = vec![layer_with_binding(b)];
        let err = validate(&cfg, Path::new("."));
        assert!(matches!(&err, Err(ConfigError::Invalid(s)) if s.contains("must not be empty")));
    }

    #[test]
    fn rejects_non_increasing_levels() {
        let mut cfg = minimal_config();
        let mut b = binding("buildings");
        b.levels = Some(vec![
            DecimationLevelConfig {
                level: 0,
                vertex_tolerance_m: 0.0,
                geometry_min_size_m: 0.0,
                label_min_priority: 0,
            },
            DecimationLevelConfig {
                level: 0,
                vertex_tolerance_m: 1.0,
                geometry_min_size_m: 1.0,
                label_min_priority: 0,
            },
        ]);
        cfg.layers = vec![layer_with_binding(b)];
        let err = validate(&cfg, Path::new("."));
        assert!(matches!(&err, Err(ConfigError::Invalid(s)) if s.contains("strictly greater")));
    }

    #[test]
    fn rejects_negative_tolerances() {
        let mut cfg = minimal_config();
        let mut b = binding("buildings");
        b.levels = Some(vec![DecimationLevelConfig {
            level: 0,
            vertex_tolerance_m: -1.0,
            geometry_min_size_m: 0.0,
            label_min_priority: 0,
        }]);
        cfg.layers = vec![layer_with_binding(b)];
        let err = validate(&cfg, Path::new("."));
        assert!(matches!(&err, Err(ConfigError::Invalid(s)) if s.contains("vertex_tolerance_m")));
    }

    #[test]
    fn rejects_zero_page_size_target() {
        let mut cfg = minimal_config();
        let mut b = binding("buildings");
        b.page_size_target_bytes = Some(0);
        cfg.layers = vec![layer_with_binding(b)];
        let err = validate(&cfg, Path::new("."));
        assert!(matches!(&err, Err(ConfigError::Invalid(s)) if s.contains("page_size_target_bytes")));
    }

    #[test]
    fn page_size_target_resolves_to_default() {
        let b = binding("buildings");
        assert_eq!(b.resolved_page_size_target(), DEFAULT_PAGE_SIZE_TARGET_BYTES);
        let mut b2 = binding("x");
        b2.page_size_target_bytes = Some(1234);
        assert_eq!(b2.resolved_page_size_target(), 1234);
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

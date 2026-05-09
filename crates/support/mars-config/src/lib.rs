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
// re-export style value types operators reach for through mars-config so
// downstream crates (mars-compiler, bin/mars) need not depend on mars-style
// just to name a `Layer.label_survival` value.
pub use mars_style::{LabelStyle, LabelSurvival, Placement, PolygonStrategy};

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

/// Validate a parsed configuration and resolve derived forms in place.
///
/// Cross-cutting checks beyond serde:
/// - every layer's `style: { ref: ... }` resolves against `styles`;
/// - every source binding's `band` (when set) exists in `scales.bands`;
/// - every `cells.size_per_band` key matches a declared band;
/// - every class `when:` parses via [`mars_expr::parse`].
///
/// Resolution step: every source binding with `band: Some(name)` has its
/// `scale: ScaleWindow` intersected with the band's half-open denominator
/// interval (SPEC §7.3, §11 Glossary — bands are routing rules). Disjoint
/// intersections are rejected so the renderer's binding picker, which
/// consumes `source.scale` directly, sees the effective routing window
/// without needing band knowledge.
///
/// `config_dir` is currently unused at validate time but accepted for symmetry
/// and future-proofing - validation may grow filesystem checks (e.g. cache
/// path writability) that require it.
pub fn validate(config: &mut Config, config_dir: &Path) -> Result<(), ConfigError> {
    let _ = config_dir;

    if config.service.name.trim().is_empty() {
        return Err(ConfigError::Invalid("service.name must not be empty".into()));
    }

    // compiler size/duration literals — fail early on bad operator config.
    let _ = config.compiler.window_dur()?;
    let working_set = config.compiler.compile_page_working_set()?;
    if working_set == 0 {
        return Err(ConfigError::Invalid(
            "compiler.compile_page_working_set_bytes must be > 0".into(),
        ));
    }
    let plan_budget = config.compiler.compile_plan_budget()?;
    if plan_budget == 0 {
        return Err(ConfigError::Invalid(
            "compiler.compile_plan_budget_bytes must be > 0".into(),
        ));
    }
    let parallelism = config.compiler.compile_binding_parallelism;
    if parallelism == 0 {
        return Err(ConfigError::Invalid(
            "compiler.compile_binding_parallelism must be > 0".into(),
        ));
    }
    if let Some(pool_max) = config.source.pool.max_size
        && parallelism > pool_max
    {
        return Err(ConfigError::Invalid(format!(
            "compiler.compile_binding_parallelism ({parallelism}) exceeds source.pool.max_size ({pool_max}); \
             raise the pool size or lower the parallelism"
        )));
    }
    let _ = config.compiler.rebalance.window_dur()?;
    if config.render.page_fetch_concurrency == 0 {
        return Err(ConfigError::Invalid(
            "render.page_fetch_concurrency must be >= 1".into(),
        ));
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

    // page-keyed substrate: cells.* is ignored; no cross-checks against bands.
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

        // class count fits in u16: class assignments are u16-indexed in the
        // sidecar artifact and the optional label's style_ref_idx is appended
        // immediately after the class style refs, so classes.len() must
        // itself fit in u16. without this check, assign_class silently
        // returns None past u16::MAX and the label style_ref_idx saturates,
        // dropping matches and aliasing styles at compile time.
        if layer.classes.len() > u16::MAX as usize {
            return Err(ConfigError::Invalid(format!(
                "layer {} declares {} classes; the per-layer limit is {}",
                layer.name,
                layer.classes.len(),
                u16::MAX
            )));
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

        // collect every attribute name the layer references via class
        // when: expressions or label.text templates. each binding declared
        // for this layer must list every referenced attribute, otherwise
        // the snapshot path would silently observe a missing column at
        // eval time.
        let mut referenced: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for class in &layer.classes {
            if let Some(when) = &class.when
                && let Ok(expr) = mars_expr::parse(when)
            {
                mars_expr::collect_idents(&expr, &mut referenced);
            }
        }
        if let Some(label) = &layer.label
            && let Ok(template) = mars_expr::parse_template(&label.text)
        {
            for seg in &template.segments {
                if let mars_expr::Segment::Ident(name) = seg {
                    referenced.insert(name.clone());
                }
            }
        }
        for (i, binding) in layer.sources.iter().enumerate() {
            let declared: std::collections::BTreeSet<&str> = binding.attributes.iter().map(String::as_str).collect();
            for name in &referenced {
                if !declared.contains(name.as_str()) {
                    return Err(ConfigError::Invalid(format!(
                        "layer {} source[{i}] (from {:?}) does not declare attribute {name:?} \
                         referenced by a class when: or label text",
                        layer.name, binding.from
                    )));
                }
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

    resolve_band_routing(config)?;

    Ok(())
}

/// Fold each source binding's declared `band` into its `scale` window.
/// Bands are ordered fine-to-coarse in `scales.bands`; band `i` covers
/// the half-open denominator interval `[prev_max, this_max)` where
/// `prev_max` is the previous band's `max_denom` (or `None` for band 0).
/// The renderer routes purely on `source.scale`, so this step is what makes
/// `band:` semantically active end-to-end.
fn resolve_band_routing(config: &mut Config) -> Result<(), ConfigError> {
    let mut band_windows: std::collections::BTreeMap<String, ScaleWindow> = std::collections::BTreeMap::new();
    let mut prev_max: Option<u64> = None;
    for band in &config.scales.bands {
        band_windows.insert(
            band.name.clone(),
            ScaleWindow {
                min: prev_max,
                max: Some(band.max_denom),
            },
        );
        prev_max = Some(band.max_denom);
    }

    for layer in &mut config.layers {
        for (i, source) in layer.sources.iter_mut().enumerate() {
            let Some(band_name) = source.band.as_ref() else {
                continue;
            };
            let band_window = band_windows.get(band_name).ok_or_else(|| {
                // band existence is already checked above; this branch is
                // defensive and should be unreachable.
                ConfigError::Invalid(format!(
                    "layer {} source[{i}] band {band_name:?} not declared in scales.bands",
                    layer.name
                ))
            })?;
            let resolved = match &source.scale {
                None => band_window.clone(),
                Some(explicit) => intersect_scale_windows(explicit, band_window).ok_or_else(|| {
                    ConfigError::Invalid(format!(
                        "layer {} source[{i}] explicit scale window {:?} is disjoint from band {band_name:?} window {:?}",
                        layer.name, explicit, band_window
                    ))
                })?,
            };
            source.scale = Some(resolved);
        }
    }

    Ok(())
}

/// Intersect two half-open scale windows. `None` bounds act as ±infinity.
/// Returns `None` if the intersection is empty (lo >= hi).
fn intersect_scale_windows(a: &ScaleWindow, b: &ScaleWindow) -> Option<ScaleWindow> {
    let min = match (a.min, b.min) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    };
    let max = match (a.max, b.max) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    };
    if let (Some(lo), Some(hi)) = (min, max)
        && lo >= hi
    {
        return None;
    }
    Some(ScaleWindow { min, max })
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
    if let Some(cycles) = binding.reconcile_every_cycles
        && cycles == 0
    {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] reconcile_every_cycles must be > 0"
        )));
    }
    let warn_bytes = binding
        .resolved_sidecar_size_warn_bytes()
        .map_err(|e| ConfigError::Invalid(format!("layer {layer} source[{idx}] sidecar_size_warn_bytes: {e}")))?;
    if warn_bytes == 0 {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] sidecar_size_warn_bytes must be > 0"
        )));
    }
    // LAZARUS Phase E line 669: the switch is wired now but `TopologyAware`
    // is the Phase 0 spike, not yet implemented. Reject explicitly so
    // operators see a clear error rather than a silent fallback to DP.
    if matches!(binding.simplifier, Some(SimplifierKind::TopologyAware)) {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] simplifier: topology_aware is not yet implemented; \
             omit the field or set simplifier: naive"
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
    let mut prev_level: Option<u8> = None;
    let mut prev_vertex_tol: Option<f64> = None;
    let mut prev_min_size: Option<f64> = None;
    let mut prev_label_prio: Option<u32> = None;
    for (li, lvl) in levels.iter().enumerate() {
        if let Some(p) = prev_level
            && lvl.level <= p
        {
            return Err(ConfigError::Invalid(format!(
                "layer {layer} source[{idx}] levels[{li}] level {} must be strictly greater than previous {}",
                lvl.level, p
            )));
        }
        prev_level = Some(lvl.level);
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
        // monotone non-decreasing across the level sequence: higher levels
        // are coarser. lets the page-rebuild path reason about decimation
        // sets as a strict refinement chain (level n's surviving features
        // are a superset of level n+1's).
        if let Some(p) = prev_vertex_tol
            && lvl.vertex_tolerance_m < p
        {
            return Err(ConfigError::Invalid(format!(
                "layer {layer} source[{idx}] levels[{li}] vertex_tolerance_m {} must be >= previous {}",
                lvl.vertex_tolerance_m, p
            )));
        }
        if let Some(p) = prev_min_size
            && lvl.geometry_min_size_m < p
        {
            return Err(ConfigError::Invalid(format!(
                "layer {layer} source[{idx}] levels[{li}] geometry_min_size_m {} must be >= previous {}",
                lvl.geometry_min_size_m, p
            )));
        }
        if let Some(p) = prev_label_prio
            && lvl.label_min_priority < p
        {
            return Err(ConfigError::Invalid(format!(
                "layer {layer} source[{idx}] levels[{li}] label_min_priority {} must be >= previous {}",
                lvl.label_min_priority, p
            )));
        }
        prev_vertex_tol = Some(lvl.vertex_tolerance_m);
        prev_min_size = Some(lvl.geometry_min_size_m);
        prev_label_prio = Some(lvl.label_min_priority);
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
            validate(&mut cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("service.name")
        ));
    }

    #[test]
    fn rejects_service_name_with_spaces() {
        let mut cfg = minimal_config();
        cfg.service.name = "foo bar".into();
        assert!(matches!(
            validate(&mut cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("spaces")
        ));
    }

    #[test]
    fn rejects_empty_native_crs() {
        let mut cfg = minimal_config();
        cfg.source.native_crs = CrsCode::new("");
        assert!(matches!(
            validate(&mut cfg, Path::new(".")),
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
            validate(&mut cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("duplicate band")
        ));
    }

    #[test]
    fn rejects_when_clause_referencing_undeclared_attribute() {
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
            sources: vec![SourceBinding {
                scale: None,
                band: None,
                from: "roads".into(),
                geometry_column: "geom".into(),
                id_column: Some("id".into()),
                attributes: vec!["name".into()],
                levels: None,
                page_size_target_bytes: None,
                reconcile_every_cycles: None,
                sidecar_size_warn_bytes: None,
                simplifier: None,
            }],
            classes: vec![crate::model::Class {
                name: "primary".into(),
                title: String::new(),
                when: Some("kind = 'major'".into()),
                style: ClassStyle::Inline(Default::default()),
            }],
            label: None,
            label_survival: mars_style::LabelSurvival::Independent,
        }];
        assert!(matches!(
            validate(&mut cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("attribute") && s.contains("kind")
        ));
    }

    #[test]
    fn rejects_label_text_referencing_undeclared_attribute() {
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
            sources: vec![SourceBinding {
                scale: None,
                band: None,
                from: "roads".into(),
                geometry_column: "geom".into(),
                id_column: Some("id".into()),
                attributes: vec!["name".into()],
                levels: None,
                page_size_target_bytes: None,
                reconcile_every_cycles: None,
                sidecar_size_warn_bytes: None,
                simplifier: None,
            }],
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
                text: "{name} ({kind})".into(),
                placement: None,
            }),
            label_survival: mars_style::LabelSurvival::Independent,
        }];
        assert!(matches!(
            validate(&mut cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("attribute") && s.contains("kind")
        ));
    }

    #[test]
    fn label_survival_defaults_to_independent_when_absent() {
        // serde default kicks in when the layer YAML has no `label_survival:` line.
        let yaml = r#"
name: roads
type: line
sources: []
"#;
        let layer: Layer = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(matches!(layer.label_survival, mars_style::LabelSurvival::Independent));
    }

    #[test]
    fn label_survival_follow_geometry_round_trips() {
        let yaml = r#"
name: roads
type: line
sources: []
label_survival: follow_geometry
"#;
        let layer: Layer = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(matches!(
            layer.label_survival,
            mars_style::LabelSurvival::FollowGeometry
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
            label_survival: mars_style::LabelSurvival::Independent,
        }];
        assert!(matches!(
            validate(&mut cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("when: parse error")
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
            label_survival: mars_style::LabelSurvival::Independent,
        }];
        assert!(matches!(
            validate(&mut cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("more than once") && s.contains("default")
        ));
    }

    #[test]
    fn rejects_layer_with_more_than_u16_max_classes() {
        let mut cfg = minimal_config();
        let classes: Vec<_> = (0..(u16::MAX as usize + 1))
            .map(|i| crate::model::Class {
                name: format!("c{i}"),
                title: String::new(),
                when: None,
                style: ClassStyle::Inline(Default::default()),
            })
            .collect();
        cfg.layers = vec![Layer {
            name: mars_types::LayerId::new("big"),
            title: String::new(),
            abstract_: String::new(),
            kind: "line".into(),
            scale: None,
            group: None,
            enable_get_feature_info: false,
            bbox: None,
            sources: vec![],
            classes,
            label: None,
            label_survival: mars_style::LabelSurvival::Independent,
        }];
        let err = validate(&mut cfg, Path::new(".")).unwrap_err();
        match err {
            ConfigError::Invalid(s) => {
                assert!(s.contains("classes"), "got: {s}");
                assert!(s.contains("65535"), "got: {s}");
            }
            other => panic!("unexpected: {other:?}"),
        }
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
            label_survival: mars_style::LabelSurvival::Independent,
        }];
        assert!(matches!(
            validate(&mut cfg, Path::new(".")),
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
            label_survival: mars_style::LabelSurvival::Independent,
        }];
        assert!(validate(&mut cfg, Path::new(".")).is_ok());
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
            label_survival: mars_style::LabelSurvival::Independent,
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
            reconcile_every_cycles: None,
            sidecar_size_warn_bytes: None,
            simplifier: None,
        }
    }

    #[test]
    fn accepts_simple_table_binding() {
        let mut cfg = minimal_config();
        cfg.layers = vec![layer_with_binding(binding("buildings"))];
        assert!(validate(&mut cfg, Path::new(".")).is_ok());

        cfg.layers = vec![layer_with_binding(binding("public.buildings"))];
        assert!(validate(&mut cfg, Path::new(".")).is_ok());
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
            let err = validate(&mut cfg, Path::new("."));
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
        let err = validate(&mut cfg, Path::new("."));
        assert!(matches!(&err, Err(ConfigError::Invalid(s)) if s.contains("schema.table")));
    }

    #[test]
    fn rejects_empty_levels() {
        let mut cfg = minimal_config();
        let mut b = binding("buildings");
        b.levels = Some(vec![]);
        cfg.layers = vec![layer_with_binding(b)];
        let err = validate(&mut cfg, Path::new("."));
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
        let err = validate(&mut cfg, Path::new("."));
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
        let err = validate(&mut cfg, Path::new("."));
        assert!(matches!(&err, Err(ConfigError::Invalid(s)) if s.contains("vertex_tolerance_m")));
    }

    #[test]
    fn rejects_decreasing_vertex_tolerance_across_levels() {
        let mut cfg = minimal_config();
        let mut b = binding("buildings");
        b.levels = Some(vec![
            DecimationLevelConfig {
                level: 0,
                vertex_tolerance_m: 5.0,
                geometry_min_size_m: 0.0,
                label_min_priority: 0,
            },
            DecimationLevelConfig {
                level: 1,
                vertex_tolerance_m: 1.0,
                geometry_min_size_m: 0.0,
                label_min_priority: 0,
            },
        ]);
        cfg.layers = vec![layer_with_binding(b)];
        let err = validate(&mut cfg, Path::new("."));
        assert!(
            matches!(&err, Err(ConfigError::Invalid(s)) if s.contains("vertex_tolerance_m") && s.contains(">= previous")),
            "expected monotone vertex_tolerance_m rejection, got {err:?}"
        );
    }

    #[test]
    fn rejects_decreasing_geometry_min_size_across_levels() {
        let mut cfg = minimal_config();
        let mut b = binding("buildings");
        b.levels = Some(vec![
            DecimationLevelConfig {
                level: 0,
                vertex_tolerance_m: 0.0,
                geometry_min_size_m: 10.0,
                label_min_priority: 0,
            },
            DecimationLevelConfig {
                level: 1,
                vertex_tolerance_m: 0.0,
                geometry_min_size_m: 1.0,
                label_min_priority: 0,
            },
        ]);
        cfg.layers = vec![layer_with_binding(b)];
        let err = validate(&mut cfg, Path::new("."));
        assert!(
            matches!(&err, Err(ConfigError::Invalid(s)) if s.contains("geometry_min_size_m") && s.contains(">= previous")),
            "expected monotone geometry_min_size_m rejection, got {err:?}"
        );
    }

    #[test]
    fn rejects_decreasing_label_min_priority_across_levels() {
        let mut cfg = minimal_config();
        let mut b = binding("buildings");
        b.levels = Some(vec![
            DecimationLevelConfig {
                level: 0,
                vertex_tolerance_m: 0.0,
                geometry_min_size_m: 0.0,
                label_min_priority: 100,
            },
            DecimationLevelConfig {
                level: 1,
                vertex_tolerance_m: 0.0,
                geometry_min_size_m: 0.0,
                label_min_priority: 50,
            },
        ]);
        cfg.layers = vec![layer_with_binding(b)];
        let err = validate(&mut cfg, Path::new("."));
        assert!(
            matches!(&err, Err(ConfigError::Invalid(s)) if s.contains("label_min_priority") && s.contains(">= previous")),
            "expected monotone label_min_priority rejection, got {err:?}"
        );
    }

    #[test]
    fn accepts_monotone_non_decreasing_levels() {
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
                level: 1,
                vertex_tolerance_m: 1.0,
                geometry_min_size_m: 5.0,
                label_min_priority: 50,
            },
            DecimationLevelConfig {
                level: 2,
                vertex_tolerance_m: 1.0, // equal is allowed
                geometry_min_size_m: 10.0,
                label_min_priority: 100,
            },
        ]);
        cfg.layers = vec![layer_with_binding(b)];
        assert!(validate(&mut cfg, Path::new(".")).is_ok());
    }

    #[test]
    fn rejects_zero_page_size_target() {
        let mut cfg = minimal_config();
        let mut b = binding("buildings");
        b.page_size_target_bytes = Some(0);
        cfg.layers = vec![layer_with_binding(b)];
        let err = validate(&mut cfg, Path::new("."));
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
    fn rejects_zero_reconcile_every_cycles() {
        let mut cfg = minimal_config();
        let mut b = binding("buildings");
        b.reconcile_every_cycles = Some(0);
        cfg.layers = vec![layer_with_binding(b)];
        let err = validate(&mut cfg, Path::new("."));
        assert!(matches!(&err, Err(ConfigError::Invalid(s)) if s.contains("reconcile_every_cycles")));
    }

    #[test]
    fn reconcile_every_cycles_resolves_to_default() {
        let b = binding("buildings");
        assert_eq!(b.resolved_reconcile_every_cycles(), DEFAULT_RECONCILE_EVERY_CYCLES);
        let mut b2 = binding("x");
        b2.reconcile_every_cycles = Some(7);
        assert_eq!(b2.resolved_reconcile_every_cycles(), 7);
    }

    #[test]
    fn rejects_unparsable_sidecar_size_warn_bytes() {
        let mut cfg = minimal_config();
        let mut b = binding("buildings");
        b.sidecar_size_warn_bytes = Some("twelve gigs".into());
        cfg.layers = vec![layer_with_binding(b)];
        let err = validate(&mut cfg, Path::new("."));
        assert!(matches!(&err, Err(ConfigError::Invalid(s)) if s.contains("sidecar_size_warn_bytes")));
    }

    #[test]
    fn sidecar_size_warn_bytes_resolves_to_default() {
        let b = binding("buildings");
        assert_eq!(
            b.resolved_sidecar_size_warn_bytes().unwrap(),
            DEFAULT_SIDECAR_SIZE_WARN_BYTES
        );
        let mut b2 = binding("x");
        b2.sidecar_size_warn_bytes = Some("12GiB".into());
        assert_eq!(b2.resolved_sidecar_size_warn_bytes().unwrap(), 12 * 1024 * 1024 * 1024);
    }

    #[test]
    fn rejects_zero_compile_page_working_set() {
        let mut cfg = minimal_config();
        cfg.compiler.compile_page_working_set_bytes = "0".into();
        let err = validate(&mut cfg, Path::new("."));
        assert!(matches!(&err, Err(ConfigError::Invalid(s)) if s.contains("compile_page_working_set_bytes")));
    }

    #[test]
    fn rejects_zero_compile_plan_budget() {
        let mut cfg = minimal_config();
        cfg.compiler.compile_plan_budget_bytes = "0".into();
        let err = validate(&mut cfg, Path::new("."));
        assert!(matches!(&err, Err(ConfigError::Invalid(s)) if s.contains("compile_plan_budget_bytes")));
    }

    #[test]
    fn rejects_unparsable_compile_plan_budget() {
        let mut cfg = minimal_config();
        cfg.compiler.compile_plan_budget_bytes = "lots".into();
        let err = validate(&mut cfg, Path::new("."));
        assert!(err.is_err());
    }

    #[test]
    fn rejects_unparsable_rebalance_window() {
        let mut cfg = minimal_config();
        cfg.compiler.rebalance.window = "every other Sunday".into();
        let err = validate(&mut cfg, Path::new("."));
        assert!(err.is_err());
    }

    #[test]
    fn compiler_defaults_round_trip() {
        let yaml = "window: 5min\ncompile_page_working_set_bytes: 512MiB\nrebalance:\n  enabled: false\n  window: 1d\n";
        let parsed: crate::model::Compiler = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(parsed.window_dur().unwrap().as_secs(), 300);
        assert_eq!(parsed.compile_page_working_set().unwrap(), 512 * 1024 * 1024);
        // unset plan_budget falls back to default 8GiB.
        assert_eq!(parsed.compile_plan_budget().unwrap(), 8u64 * 1024 * 1024 * 1024);
        assert!(!parsed.rebalance.enabled);
        assert_eq!(parsed.rebalance.window_dur().unwrap().as_secs(), 24 * 3600);
    }

    #[test]
    fn simplifier_defaults_to_naive() {
        let b = binding("buildings");
        assert_eq!(b.resolved_simplifier(), SimplifierKind::Naive);
    }

    #[test]
    fn rejects_topology_aware_simplifier_until_phase0_lands() {
        let mut cfg = minimal_config();
        let mut b = binding("buildings");
        b.simplifier = Some(SimplifierKind::TopologyAware);
        cfg.layers = vec![layer_with_binding(b)];
        let err = validate(&mut cfg, Path::new("."));
        assert!(
            matches!(&err, Err(ConfigError::Invalid(s)) if s.contains("topology_aware")),
            "expected topology_aware rejection, got {err:?}"
        );
    }

    #[test]
    fn simplifier_naive_yaml_roundtrip() {
        let yaml = "naive";
        let parsed: SimplifierKind = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(parsed, SimplifierKind::Naive);
        let yaml = "topology_aware";
        let parsed: SimplifierKind = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(parsed, SimplifierKind::TopologyAware);
        let yaml = "guesswork";
        assert!(serde_yaml_ng::from_str::<SimplifierKind>(yaml).is_err());
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
            label_survival: mars_style::LabelSurvival::Independent,
        };
        cfg.layers = vec![layer.clone(), layer];
        assert!(matches!(
            validate(&mut cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("duplicate layer")
        ));
    }

    fn two_band_config() -> Config {
        let mut cfg = minimal_config();
        cfg.scales.bands = vec![
            Band {
                name: "hi".into(),
                max_denom: 25_000,
            },
            Band {
                name: "mid".into(),
                max_denom: 250_000,
            },
        ];
        cfg.cells.size_per_band.insert("mid".into(), "4096m".into());
        cfg
    }

    #[test]
    fn band_resolves_to_scale_window_for_first_band() {
        let mut cfg = two_band_config();
        let mut b = binding("buildings");
        b.band = Some("hi".into());
        cfg.layers = vec![layer_with_binding(b)];
        validate(&mut cfg, Path::new(".")).expect("validate");
        let scale = cfg.layers[0].sources[0].scale.as_ref().expect("scale set");
        assert_eq!(scale.min, None);
        assert_eq!(scale.max, Some(25_000));
    }

    #[test]
    fn band_resolves_to_scale_window_for_middle_band() {
        let mut cfg = two_band_config();
        let mut b = binding("buildings");
        b.band = Some("mid".into());
        cfg.layers = vec![layer_with_binding(b)];
        validate(&mut cfg, Path::new(".")).expect("validate");
        let scale = cfg.layers[0].sources[0].scale.as_ref().expect("scale set");
        assert_eq!(scale.min, Some(25_000));
        assert_eq!(scale.max, Some(250_000));
    }

    #[test]
    fn band_intersects_with_explicit_scale() {
        let mut cfg = two_band_config();
        let mut b = binding("buildings");
        b.band = Some("mid".into());
        b.scale = Some(ScaleWindow {
            min: Some(50_000),
            max: Some(200_000),
        });
        cfg.layers = vec![layer_with_binding(b)];
        validate(&mut cfg, Path::new(".")).expect("validate");
        let scale = cfg.layers[0].sources[0].scale.as_ref().expect("scale set");
        assert_eq!(scale.min, Some(50_000));
        assert_eq!(scale.max, Some(200_000));
    }

    #[test]
    fn band_disjoint_with_explicit_scale_rejected() {
        let mut cfg = two_band_config();
        let mut b = binding("buildings");
        b.band = Some("hi".into());
        // hi covers [0, 25_000); explicit window starts at 50_000.
        b.scale = Some(ScaleWindow {
            min: Some(50_000),
            max: Some(100_000),
        });
        cfg.layers = vec![layer_with_binding(b)];
        let err = validate(&mut cfg, Path::new(".")).unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid(ref s) if s.contains("disjoint")),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn band_intersects_with_open_explicit_scale() {
        let mut cfg = two_band_config();
        let mut b = binding("buildings");
        b.band = Some("mid".into());
        // explicit min only; expect mid window's max to win and explicit min
        // to clamp the lower edge above the band's natural min.
        b.scale = Some(ScaleWindow {
            min: Some(100_000),
            max: None,
        });
        cfg.layers = vec![layer_with_binding(b)];
        validate(&mut cfg, Path::new(".")).expect("validate");
        let scale = cfg.layers[0].sources[0].scale.as_ref().expect("scale set");
        assert_eq!(scale.min, Some(100_000));
        assert_eq!(scale.max, Some(250_000));
    }

    #[test]
    fn no_band_leaves_explicit_scale_untouched() {
        let mut cfg = two_band_config();
        let mut b = binding("buildings");
        b.band = None;
        b.scale = Some(ScaleWindow {
            min: Some(10),
            max: Some(20),
        });
        cfg.layers = vec![layer_with_binding(b)];
        validate(&mut cfg, Path::new(".")).expect("validate");
        let scale = cfg.layers[0].sources[0].scale.as_ref().expect("scale set");
        assert_eq!(scale.min, Some(10));
        assert_eq!(scale.max, Some(20));
    }
}

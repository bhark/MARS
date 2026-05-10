use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::ConfigError;
use crate::model::*;

mod band;
mod binding;
mod crs;

#[cfg(test)]
pub mod fixtures;

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
    if !config.service.scale_dpi.is_finite() || config.service.scale_dpi <= 0.0 {
        return Err(ConfigError::Invalid(format!(
            "service.scale_dpi must be a positive, finite number; got {}",
            config.service.scale_dpi
        )));
    }

    let crs = config.source.native_crs.as_str().trim();
    if crs.is_empty() {
        return Err(ConfigError::Invalid("source.native_crs must not be empty".into()));
    }
    if !crs::is_metric_crs(crs)? {
        return Err(ConfigError::Invalid(format!(
            "source.native_crs {:?} is not a recognised metric CRS; mars-runtime requires a metric canonical CRS \
             (units-per-metre = 1). Use a projected, metre-based EPSG code (e.g. EPSG:25832, EPSG:3857).",
            crs
        )));
    }

    let mut band_names = BTreeSet::new();
    let mut band_windows: BTreeMap<String, ScaleWindow> = BTreeMap::new();
    let mut prev_max: Option<u64> = None;
    for band in &config.scales.bands {
        if !band_names.insert(band.name.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "duplicate band name {:?} in scales.bands",
                band.name
            )));
        }
        band_windows.insert(
            band.name.clone(),
            ScaleWindow {
                min: prev_max,
                max: Some(band.max_denom),
            },
        );
        prev_max = Some(band.max_denom);
    }

    // page-keyed substrate: cells.* is ignored; no cross-checks against bands.
    let mut layer_names = BTreeSet::new();
    for layer in &config.layers {
        if !layer_names.insert(layer.name.as_str()) {
            return Err(ConfigError::Invalid(format!("duplicate layer name {:?}", layer.name)));
        }

        // class names must be unique within a layer; a duplicate makes the
        // second class unreachable (first-match wins) which is almost never
        // the operator's intent.
        let mut class_names = BTreeSet::new();
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

            if binding.max_denom.is_some() && binding.band.is_none() {
                return Err(ConfigError::Invalid(format!(
                    "layer {} source[{i}] max_denom_exclusive requires a band",
                    layer.name
                )));
            }

            binding::validate_binding_from(&layer.name, i, &binding.from)?;
            binding::validate_binding_levels(&layer.name, i, binding)?;
        }

        band::validate_band_tiers(layer, &band_windows)?;

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

            // class scale window must be a valid half-open interval; if the
            // layer carries its own window the class window must intersect it
            // (a class wholly outside the layer window can never fire).
            if let Some(cs) = &class.scale {
                match (cs.min, cs.max) {
                    (Some(a), Some(b)) if a >= b => {
                        return Err(ConfigError::Invalid(format!(
                            "layer {} class {:?} scale window is empty: min {a} >= max {b}",
                            layer.name, class.name
                        )));
                    }
                    _ => {}
                }
                if let Some(ls) = &layer.scale
                    && band::intersect_scale_windows(ls, cs).is_none()
                {
                    return Err(ConfigError::Invalid(format!(
                        "layer {} class {:?} scale window is disjoint from layer scale window",
                        layer.name, class.name
                    )));
                }
            }
        }

        // collect every attribute name the layer references via class
        // when: expressions or label.text templates. each binding declared
        // for this layer must list every referenced attribute, otherwise
        // the snapshot path would silently observe a missing column at
        // eval time.
        let mut referenced: BTreeSet<String> = BTreeSet::new();
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
            let declared: BTreeSet<&str> = binding.attributes.iter().map(String::as_str).collect();
            for name in &referenced {
                if !declared.contains(name.as_str()) {
                    return Err(ConfigError::Invalid(format!(
                        "layer {} source[{i}] (from {:?}) does not declare attribute {name:?} \
                         referenced by a class when: or label text",
                        layer.name, binding.from
                    )));
                }
            }
            // binding-level filter: must parse, and every identifier it
            // references must be declared (attributes or id_column). this
            // mirrors the lower_to_sql allowlist in mars-source-postgres so a
            // bad filter fails loudly at config time instead of at SQL build.
            if let Some(f) = &binding.filter {
                let expr = mars_expr::parse(f).map_err(|e| {
                    ConfigError::Invalid(format!(
                        "layer {} source[{i}] (from {:?}) filter parse error: {e}",
                        layer.name, binding.from
                    ))
                })?;
                let mut idents: BTreeSet<String> = BTreeSet::new();
                mars_expr::collect_idents(&expr, &mut idents);
                for name in &idents {
                    let in_attrs = declared.contains(name.as_str());
                    let is_id = binding.id_column.as_deref() == Some(name.as_str());
                    if !in_attrs && !is_id {
                        return Err(ConfigError::Invalid(format!(
                            "layer {} source[{i}] (from {:?}) filter references unknown ident {name:?}; \
                             declare it in `attributes` or as `id_column`",
                            layer.name, binding.from
                        )));
                    }
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

    band::resolve_band_routing(config)?;

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::path::Path;

    use crate::model::{Band, ClassStyle, LabelStyleAttach, Layer, LayerLabel, SourceBinding};
    use crate::validate::fixtures::*;
    use crate::validate::validate;
    use crate::ConfigError;
    use mars_types::CrsCode;

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
                max_denom: None,
                filter: None,
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
                scale: None,
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
    fn rejects_binding_filter_with_undeclared_ident() {
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
                max_denom: None,
                filter: Some("midtebredde IN ('12-', '2.5-12')".into()),
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
            label: None,
            label_survival: mars_style::LabelSurvival::Independent,
        }];
        let err = validate(&mut cfg, Path::new(".")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("filter"), "expected filter error: {msg}");
        assert!(msg.contains("midtebredde"), "expected ident name: {msg}");
    }

    #[test]
    fn accepts_binding_filter_when_ident_declared() {
        let mut cfg = minimal_config();
        cfg.layers = vec![Layer {
            name: mars_types::LayerId::new("streams"),
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
                max_denom: None,
                filter: Some("midtebredde IN ('12-', '2.5-12')".into()),
                from: "streams".into(),
                geometry_column: "geom".into(),
                id_column: Some("id".into()),
                attributes: vec!["midtebredde".into()],
                levels: None,
                page_size_target_bytes: None,
                reconcile_every_cycles: None,
                sidecar_size_warn_bytes: None,
                simplifier: None,
            }],
            classes: vec![],
            label: None,
            label_survival: mars_style::LabelSurvival::Independent,
        }];
        validate(&mut cfg, Path::new(".")).expect("filter referencing declared attribute should validate");
    }

    #[test]
    fn rejects_label_text_referencing_undeclared_attribute() {
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
                max_denom: None,
                filter: None,
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
                text: "{missing}".into(),
                style: LabelStyleAttach::Inline(mars_style::LabelStyle {
                    font_family: "sans".into(),
                    font_size: 12.0,
                    fill: mars_style::Colour::rgb(0, 0, 0),
                    halo: None,
                    priority: 0,
                    min_distance: 0.0,
                }),
                placement: None,
            }),
            label_survival: mars_style::LabelSurvival::Independent,
        }];
        let err = validate(&mut cfg, Path::new(".")).unwrap_err();
        assert!(
            err.to_string().contains("missing"),
            "expected missing attribute error: {err}"
        );
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
                    scale: None,
                    style: ClassStyle::Inline(Default::default()),
                },
                crate::model::Class {
                    name: "default".into(),
                    title: String::new(),
                    when: None,
                    scale: None,
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
                scale: None,
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
    fn rejects_placement_geom_mismatch() {
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
}

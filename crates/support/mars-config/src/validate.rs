use std::path::Path;

use crate::ConfigError;
use crate::model::Config;

mod attributes;
mod band;
mod binding;
mod class;
mod compiler;
mod crs;
mod label;
mod layer;
mod service;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub(crate) mod fixtures;

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
/// interval (Glossary - bands are routing rules). Disjoint intersections are
/// rejected so the renderer's binding picker, which consumes `source.scale`
/// directly, sees the effective routing window without needing band knowledge.
///
/// `config_dir` is currently unused at validate time but accepted for symmetry
/// and future-proofing - validation may grow filesystem checks (e.g. cache
/// path writability) that require it.
pub fn validate(config: &mut Config, config_dir: &Path) -> Result<(), ConfigError> {
    let _ = config_dir;

    service::validate_service(config)?;
    compiler::validate_compiler_and_render(config)?;
    crs::validate_native_crs(config)?;

    let bands = band::validate_bands(config)?;
    layer::validate_layers(config, &bands)?;

    band::resolve_band_routing(config)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::path::Path;

    use crate::ConfigError;
    use crate::model::Band;
    use crate::validate::fixtures::*;
    use crate::validate::validate;
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
        let mut b = binding("roads");
        b.attributes = vec!["name".into()];
        let mut l = layer("roads");
        l.sources = vec![b];
        l.classes = vec![class_inline("primary", Some("kind = 'major'"))];
        cfg.layers = vec![l];
        assert!(matches!(
            validate(&mut cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("attribute") && s.contains("kind")
        ));
    }

    #[test]
    fn rejects_binding_filter_with_undeclared_ident() {
        let mut cfg = minimal_config();
        let mut b = binding("roads");
        b.filter = Some("midtebredde IN ('12-', '2.5-12')".into());
        b.attributes = vec!["name".into()];
        let mut l = layer("roads");
        l.sources = vec![b];
        cfg.layers = vec![l];
        let err = validate(&mut cfg, Path::new(".")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("filter"), "expected filter error: {msg}");
        assert!(msg.contains("midtebredde"), "expected ident name: {msg}");
    }

    #[test]
    fn accepts_binding_filter_when_ident_declared() {
        let mut cfg = minimal_config();
        let mut b = binding("streams");
        b.filter = Some("midtebredde IN ('12-', '2.5-12')".into());
        b.attributes = vec!["midtebredde".into()];
        let mut l = layer("streams");
        l.sources = vec![b];
        cfg.layers = vec![l];
        validate(&mut cfg, Path::new(".")).expect("filter referencing declared attribute should validate");
    }

    #[test]
    fn rejects_label_text_referencing_undeclared_attribute() {
        let mut cfg = minimal_config();
        let mut b = binding("roads");
        b.attributes = vec!["name".into()];
        let mut l = layer("roads");
        l.sources = vec![b];
        l.label = Some(inline_label("{missing}", None));
        cfg.layers = vec![l];
        let err = validate(&mut cfg, Path::new(".")).unwrap_err();
        assert!(
            err.to_string().contains("missing"),
            "expected missing attribute error: {err}"
        );
    }

    #[test]
    fn rejects_duplicate_class_names_within_layer() {
        let mut cfg = minimal_config();
        let mut l = layer("roads");
        l.classes = vec![class_inline("default", None), class_inline("default", None)];
        cfg.layers = vec![l];
        assert!(matches!(
            validate(&mut cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("more than once") && s.contains("default")
        ));
    }

    #[test]
    fn rejects_layer_with_more_than_u16_max_classes() {
        let mut cfg = minimal_config();
        let classes: Vec<_> = (0..(u16::MAX as usize + 1))
            .map(|i| class_inline(&format!("c{i}"), None))
            .collect();
        let mut l = layer("big");
        l.classes = classes;
        cfg.layers = vec![l];
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
        let mut l = layer("roads");
        l.kind = "polygon".into();
        l.label = Some(inline_label(
            "{name}",
            Some(mars_style::Placement::Line {
                repeat_m: 250.0,
                max_angle_delta_deg: 25.0,
            }),
        ));
        cfg.layers = vec![l];
        assert!(matches!(
            validate(&mut cfg, Path::new(".")),
            Err(ConfigError::Invalid(ref s)) if s.contains("placement does not match")
        ));
    }

    #[test]
    fn accepts_placement_matching_geom() {
        let mut cfg = minimal_config();
        let mut l = layer("roads");
        l.label = Some(inline_label(
            "{name}",
            Some(mars_style::Placement::Line {
                repeat_m: 250.0,
                max_angle_delta_deg: 25.0,
            }),
        ));
        cfg.layers = vec![l];
        assert!(validate(&mut cfg, Path::new(".")).is_ok());
    }

    #[test]
    fn rejects_duplicate_layer_names() {
        let mut cfg = minimal_config();
        let l = layer("roads");
        cfg.layers = vec![l.clone(), l];
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

use mars_types::LayerId;

use crate::ConfigError;
use crate::model::{SimplifierKind, SourceBinding};

/// Reject `from:` strings that are not a single change-feed-mappable table.
/// v1 restriction: a binding must point at one
/// real table or a single-table view so pgoutput events map to a single
/// feature_id. Multi-table joins, embedded SELECTs, or compound DDL fragments
/// are rejected here, far from the snapshot path that would otherwise fail
/// opaquely later.
pub(super) fn validate_binding_from(layer: &LayerId, idx: usize, from: &str) -> Result<(), ConfigError> {
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
pub(super) fn validate_binding_levels(layer: &LayerId, idx: usize, binding: &SourceBinding) -> Result<(), ConfigError> {
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
    // Topology-aware simplification is not yet implemented. `TopologyAware`
    // is the spike target, not yet ready. Reject explicitly so operators see
    // a clear error rather than a silent fallback to naive DP.
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::path::Path;

    use crate::SimplifierKind;
    use crate::model::DecimationLevelConfig;
    use crate::validate::fixtures::*;
    use crate::validate::validate;

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
                matches!(&err, Err(crate::ConfigError::Invalid(s)) if s.contains("single-table")),
                "expected single-table rejection for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn rejects_overdotted_from() {
        let mut cfg = minimal_config();
        cfg.layers = vec![layer_with_binding(binding("a.b.c"))];
        let err = validate(&mut cfg, Path::new("."));
        assert!(matches!(&err, Err(crate::ConfigError::Invalid(s)) if s.contains("schema.table")));
    }

    #[test]
    fn rejects_empty_levels() {
        let mut cfg = minimal_config();
        let mut b = binding("buildings");
        b.levels = Some(vec![]);
        cfg.layers = vec![layer_with_binding(b)];
        let err = validate(&mut cfg, Path::new("."));
        assert!(matches!(&err, Err(crate::ConfigError::Invalid(s)) if s.contains("must not be empty")));
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
        assert!(matches!(&err, Err(crate::ConfigError::Invalid(s)) if s.contains("strictly greater")));
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
        assert!(matches!(&err, Err(crate::ConfigError::Invalid(s)) if s.contains("vertex_tolerance_m")));
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
            matches!(&err, Err(crate::ConfigError::Invalid(s)) if s.contains("vertex_tolerance_m") && s.contains(">= previous")),
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
            matches!(&err, Err(crate::ConfigError::Invalid(s)) if s.contains("geometry_min_size_m") && s.contains(">= previous")),
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
            matches!(&err, Err(crate::ConfigError::Invalid(s)) if s.contains("label_min_priority") && s.contains(">= previous")),
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
        assert!(matches!(&err, Err(crate::ConfigError::Invalid(s)) if s.contains("page_size_target_bytes")));
    }

    #[test]
    fn rejects_zero_reconcile_every_cycles() {
        let mut cfg = minimal_config();
        let mut b = binding("buildings");
        b.reconcile_every_cycles = Some(0);
        cfg.layers = vec![layer_with_binding(b)];
        let err = validate(&mut cfg, Path::new("."));
        assert!(matches!(&err, Err(crate::ConfigError::Invalid(s)) if s.contains("reconcile_every_cycles")));
    }

    #[test]
    fn rejects_unparsable_sidecar_size_warn_bytes() {
        let mut cfg = minimal_config();
        let mut b = binding("buildings");
        b.sidecar_size_warn_bytes = Some("twelve gigs".into());
        cfg.layers = vec![layer_with_binding(b)];
        let err = validate(&mut cfg, Path::new("."));
        assert!(matches!(&err, Err(crate::ConfigError::Invalid(s)) if s.contains("sidecar_size_warn_bytes")));
    }

    #[test]
    fn rejects_topology_aware_simplifier_until_phase0_lands() {
        let mut cfg = minimal_config();
        let mut b = binding("buildings");
        b.simplifier = Some(SimplifierKind::TopologyAware);
        cfg.layers = vec![layer_with_binding(b)];
        let err = validate(&mut cfg, Path::new("."));
        assert!(
            matches!(&err, Err(crate::ConfigError::Invalid(s)) if s.contains("topology_aware")),
            "expected topology_aware rejection, got {err:?}"
        );
    }
}

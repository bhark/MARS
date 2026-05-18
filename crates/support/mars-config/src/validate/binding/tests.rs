#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

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
fn rejects_both_from_and_sql_set() {
    let mut cfg = minimal_config();
    let mut b = binding("table");
    b.sql = Some("SELECT 1".into());
    cfg.layers = vec![layer_with_binding(b)];
    let err = validate(&mut cfg, Path::new(".")).unwrap_err();
    assert!(
        matches!(&err, crate::ConfigError::Invalid(s) if s.contains("mutually exclusive")),
        "expected mutually-exclusive rejection, got {err:?}"
    );
}

#[test]
fn rejects_neither_from_nor_sql_set() {
    let mut cfg = minimal_config();
    let mut b = binding("table");
    b.from = None;
    cfg.layers = vec![layer_with_binding(b)];
    let err = validate(&mut cfg, Path::new(".")).unwrap_err();
    assert!(
        matches!(&err, crate::ConfigError::Invalid(s) if s.contains("exactly one")),
        "expected one-of rejection, got {err:?}"
    );
}

#[test]
fn accepts_sql_binding() {
    let mut cfg = minimal_config();
    let mut b = binding("ignored");
    b.from = None;
    b.sql = Some("SELECT id, geom FROM a JOIN b USING (k)".into());
    cfg.layers = vec![layer_with_binding(b)];
    validate(&mut cfg, Path::new(".")).expect("sql binding accepted");
}

#[test]
fn rejects_sql_with_semicolon() {
    let mut cfg = minimal_config();
    let mut b = binding("ignored");
    b.from = None;
    b.sql = Some("SELECT 1; DROP TABLE x".into());
    cfg.layers = vec![layer_with_binding(b)];
    let err = validate(&mut cfg, Path::new(".")).unwrap_err();
    assert!(
        matches!(&err, crate::ConfigError::Invalid(s) if s.contains("only a single SELECT")),
        "expected semicolon rejection, got {err:?}"
    );
}

#[test]
fn rejects_sql_without_select_prefix() {
    let mut cfg = minimal_config();
    let mut b = binding("ignored");
    b.from = None;
    b.sql = Some("UPDATE t SET x = 1".into());
    cfg.layers = vec![layer_with_binding(b)];
    let err = validate(&mut cfg, Path::new(".")).unwrap_err();
    assert!(
        matches!(&err, crate::ConfigError::Invalid(s) if s.contains("must be a SELECT")),
        "expected SELECT requirement, got {err:?}"
    );
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
fn accepts_postgis_binding_with_dsn_override() {
    let mut cfg = minimal_config();
    let mut b = binding("buildings");
    b.dsn = Some("postgres://override@host/db".into());
    cfg.layers = vec![layer_with_binding(b)];
    assert!(
        validate(&mut cfg, Path::new(".")).is_ok(),
        "postgis binding with dsn override must validate"
    );
}

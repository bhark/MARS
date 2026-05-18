use std::num::NonZeroUsize;

use crate::SourceId;
use crate::model::{BindingKind, Compiler, PngCompression, Render, SimplifierKind, SourceBinding};
use crate::model::{DEFAULT_PAGE_SIZE_TARGET_BYTES, DEFAULT_RECONCILE_EVERY_CYCLES, DEFAULT_SIDECAR_SIZE_WARN_BYTES};

fn minimal_binding() -> SourceBinding {
    SourceBinding {
        source: SourceId::new("default"),
        kind: BindingKind::PostgisTable {
            from: "x".into(),
            geometry_column: "geom".into(),
            dsn: None,
        },
        scale: None,
        band: None,
        max_denom: None,
        filter: None,
        id_column: Some("id".into()),
        attributes: vec![],
        levels: None,
        page_size_target_bytes: None,
        reconcile_every_cycles: None,
        sidecar_size_warn_bytes: None,
        simplifier: None,
        on_missing_page: None,
    }
}

#[test]
fn compiler_parallel_cells_yaml_roundtrip() {
    // unset → None
    let yaml = "window: 5min\n";
    let c: Compiler = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(c.parallel_cells.is_none());

    // explicit positive value → Some(N)
    let yaml = "window: 5min\nparallel_cells: 8\n";
    let c: Compiler = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(c.parallel_cells.map(NonZeroUsize::get), Some(8));

    // zero must be rejected at parse (NonZeroUsize)
    let yaml = "window: 5min\nparallel_cells: 0\n";
    assert!(serde_yaml_ng::from_str::<Compiler>(yaml).is_err());
}

#[test]
fn render_png_compression_yaml_roundtrip() {
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
fn page_size_target_resolves_to_default() {
    let b = minimal_binding();
    assert_eq!(b.resolved_page_size_target(), DEFAULT_PAGE_SIZE_TARGET_BYTES);
    let mut b2 = minimal_binding();
    b2.page_size_target_bytes = Some(1234);
    assert_eq!(b2.resolved_page_size_target(), 1234);
}

#[test]
fn reconcile_every_cycles_resolves_to_default() {
    let b = minimal_binding();
    assert_eq!(b.resolved_reconcile_every_cycles(), DEFAULT_RECONCILE_EVERY_CYCLES);
    let mut b2 = minimal_binding();
    b2.reconcile_every_cycles = Some(7);
    assert_eq!(b2.resolved_reconcile_every_cycles(), 7);
}

#[test]
fn sidecar_size_warn_bytes_resolves_to_default() {
    let b = minimal_binding();
    assert_eq!(
        b.resolved_sidecar_size_warn_bytes().unwrap(),
        DEFAULT_SIDECAR_SIZE_WARN_BYTES
    );
    let mut b2 = minimal_binding();
    b2.sidecar_size_warn_bytes = Some("12GiB".into());
    assert_eq!(b2.resolved_sidecar_size_warn_bytes().unwrap(), 12 * 1024 * 1024 * 1024);
}

#[test]
fn compiler_defaults_round_trip() {
    let yaml = "window: 5min\ncompile_page_working_set_bytes: 512MiB\nrebalance:\n  enabled: false\n  window: 1d\n";
    let parsed: Compiler = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(parsed.window_dur().unwrap().as_secs(), 300);
    assert_eq!(parsed.compile_page_working_set().unwrap(), 512 * 1024 * 1024);
    // unset plan_budget falls back to default 8GiB.
    assert_eq!(parsed.compile_plan_budget().unwrap(), 8u64 * 1024 * 1024 * 1024);
    assert!(!parsed.rebalance.enabled);
    assert_eq!(parsed.rebalance.window_dur().unwrap().as_secs(), 24 * 3600);
}

#[test]
fn simplifier_defaults_to_naive() {
    let b = minimal_binding();
    assert_eq!(b.resolved_simplifier(), SimplifierKind::Naive);
}

#[test]
fn simplifier_naive_yaml_roundtrip() {
    let yaml = "naive";
    let parsed: SimplifierKind = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(parsed, SimplifierKind::Naive);
    let yaml = "guesswork";
    assert!(serde_yaml_ng::from_str::<SimplifierKind>(yaml).is_err());
}

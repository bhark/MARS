#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;

use mars_config::{ConfigError, load, validate};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")
}

#[test]
fn loads_minimal_fixture() {
    let path = fixtures_dir().join("demo_minimal.yaml");
    let mut cfg = load(&path).expect("load minimal");
    assert_eq!(cfg.service.name, "demo");
    assert_eq!(cfg.scales.bands.len(), 1);
    assert_eq!(cfg.layers.len(), 1);
    assert_eq!(cfg.layers[0].classes.len(), 2);

    let cache_bytes = cfg.artifacts.cache.max_size_bytes().unwrap();
    assert_eq!(cache_bytes, 50u64 * 1024 * 1024 * 1024);

    let sizes = cfg.cells.size_per_band_m().unwrap();
    assert!((sizes["hi"] - 4096.0).abs() < f64::EPSILON);

    // unset scale_dpi defaults to 96 (mapserver-parity).
    assert!((cfg.service.scale_dpi - 96.0).abs() < f64::EPSILON);
    assert!((cfg.service.scale_pixel_size_m() - 0.0254 / 96.0).abs() < 1e-12);

    validate(&mut cfg, &fixtures_dir()).expect("validate minimal");
}

#[test]
fn rejects_non_positive_scale_dpi() {
    let dir = tempfile::tempdir().unwrap();
    let yaml = r#"
service: { name: t, scale_dpi: 0 }
sources:
  - id: pg
    type: postgis
    dsn: postgres://example/x
    native_crs: EPSG:25832
artifacts:
  store: { type: fs, path: /tmp/s }
  cache: { path: /tmp/c, max_size: 1MiB }
scales:
  bands:
    - { name: hi, max_denom_exclusive: 25000 }
cells: { grid: regular, origin: [0, 0], size_per_band: { hi: 1024m } }
interfaces: {}
layers: []
"#;
    let p = dir.path().join("bad.yaml");
    fs::write(&p, yaml).unwrap();
    let mut cfg = load(&p).expect("load");
    let err = validate(&mut cfg, dir.path()).unwrap_err();
    assert!(matches!(err, ConfigError::Invalid(msg) if msg.contains("scale_dpi")));
}

#[test]
fn missing_style_ref_is_rejected() {
    let path = fixtures_dir().join("demo_minimal.yaml");
    let mut cfg = load(&path).unwrap();
    if let mars_config::ClassStyle::Ref { name } = &mut cfg.layers[0].classes[0].style {
        *name = "no_such_style".into();
    } else {
        panic!("expected ref");
    }
    let err = validate(&mut cfg, &fixtures_dir()).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("bygning"), "error should name layer: {msg}");
    assert!(msg.contains("no_such_style"), "error should name style: {msg}");
}

#[test]
fn non_metric_canonical_crs_is_rejected() {
    let path = fixtures_dir().join("demo_minimal.yaml");
    let mut cfg = load(&path).unwrap();
    // EPSG:4326 is geographic (lat/lon, degrees) - must be refused at load time.
    cfg.sources[0].native_crs = mars_types::CrsCode::new("EPSG:4326");
    let err = validate(&mut cfg, &fixtures_dir()).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("metric"), "error should mention metric: {msg}");
    assert!(msg.contains("EPSG:4326"), "error should name the rejected crs: {msg}");
}

#[test]
fn unknown_band_in_source_binding_is_rejected() {
    let path = fixtures_dir().join("demo_minimal.yaml");
    let mut cfg = load(&path).unwrap();
    cfg.layers[0].sources[0].band = Some("ghost".into());
    let err = validate(&mut cfg, &fixtures_dir()).unwrap_err();
    assert!(err.to_string().contains("ghost"));
}

#[test]
fn env_default_used_when_unset() {
    temp_env::with_var_unset("MARS_TEST_DSN", || {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
service: { name: t }
source:
  type: postgis
  dsn: ${MARS_TEST_DSN:-postgres://default/x}
  native_crs: EPSG:25832
artifacts:
  store: { type: fs, path: /tmp/s }
  cache: { path: /tmp/c, max_size: 1MiB }
scales: { bands: [{ name: hi, max_denom_exclusive: 1 }] }
cells: { grid: regular, origin: [0, 0], size_per_band: { hi: 1m } }
interfaces: {}
"#;
        let p = dir.path().join("c.yaml");
        fs::write(&p, yaml).unwrap();
        let cfg = load(&p).unwrap();
        assert_eq!(cfg.sources[0].postgis().unwrap().dsn, "postgres://default/x");
    });
}

#[test]
fn env_unset_no_default_errors() {
    temp_env::with_var_unset("MARS_TEST_REQUIRED_VAR", || {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
service: { name: t }
source:
  type: postgis
  dsn: ${MARS_TEST_REQUIRED_VAR}
  native_crs: EPSG:25832
artifacts:
  store: { type: fs, path: /tmp/s }
  cache: { path: /tmp/c, max_size: 1MiB }
scales: { bands: [{ name: hi, max_denom_exclusive: 1 }] }
cells: { grid: regular, origin: [0, 0], size_per_band: { hi: 1m } }
interfaces: {}
"#;
        let p = dir.path().join("c.yaml");
        fs::write(&p, yaml).unwrap();
        let err = load(&p).unwrap_err();
        assert!(matches!(err, ConfigError::EnvMissing(name) if name == "MARS_TEST_REQUIRED_VAR"));
    });
}

#[test]
fn env_in_yaml_comment_is_ignored() {
    temp_env::with_var_unset("MARS_TEST_COMMENTED_OUT", || {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
service: { name: t }
source:
  type: postgis
  # historical: dsn: ${MARS_TEST_COMMENTED_OUT}
  dsn: postgres://example/x
  native_crs: EPSG:25832
artifacts:
  store: { type: fs, path: /tmp/s }
  cache: { path: /tmp/c, max_size: 1MiB }
scales: { bands: [{ name: hi, max_denom_exclusive: 1 }] }
cells: { grid: regular, origin: [0, 0], size_per_band: { hi: 1m } }
interfaces: {}
"#;
        let p = dir.path().join("c.yaml");
        fs::write(&p, yaml).unwrap();
        let cfg = load(&p).unwrap();
        assert_eq!(cfg.sources[0].postgis().unwrap().dsn, "postgres://example/x");
    });
}

#[test]
fn include_resolves_relative() {
    let dir = tempfile::tempdir().unwrap();
    let main = r#"
service: { name: t }
source:
  type: postgis
  dsn: x
  native_crs: EPSG:25832
artifacts:
  store: { type: fs, path: /tmp/s }
  cache: { path: /tmp/c, max_size: 1MiB }
scales: { bands: [{ name: hi, max_denom_exclusive: 1 }] }
cells: { grid: regular, origin: [0, 0], size_per_band: { hi: 1m } }
interfaces: {}
layers: !include layers.yaml
"#;
    let layers = r#"
- name: foo
  type: polygon
  sources:
    - band: hi
      from: t.foo
      geometry_column: geom
  classes: []
"#;
    fs::write(dir.path().join("c.yaml"), main).unwrap();
    fs::write(dir.path().join("layers.yaml"), layers).unwrap();
    let cfg = load(dir.path().join("c.yaml")).unwrap();
    assert_eq!(cfg.layers.len(), 1);
    assert_eq!(cfg.layers[0].name.as_str(), "foo");
}

#[test]
fn include_escapes_config_dir_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let main = r#"
service: { name: t }
source: { type: postgis, dsn: x, native_crs: EPSG:25832 }
artifacts:
  store: { type: fs, path: /tmp/s }
  cache: { path: /tmp/c, max_size: 1MiB }
scales: { bands: [{ name: hi, max_denom_exclusive: 1 }] }
cells: { grid: regular, origin: [0, 0], size_per_band: { hi: 1m } }
interfaces: {}
layers: !include /etc/passwd
"#;
    fs::write(dir.path().join("c.yaml"), main).unwrap();
    let err = load(dir.path().join("c.yaml")).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("escapes"), "expected escape error, got {msg}");
}

#[test]
fn include_cycle_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    // a.yaml includes b.yaml under layers; b.yaml includes a.yaml under layers.
    fs::write(
        dir.path().join("a.yaml"),
        r#"
service: { name: t }
source: { type: postgis, dsn: x, native_crs: EPSG:25832 }
artifacts:
  store: { type: fs, path: /tmp/s }
  cache: { path: /tmp/c, max_size: 1MiB }
scales: { bands: [{ name: hi, max_denom_exclusive: 1 }] }
cells: { grid: regular, origin: [0, 0], size_per_band: { hi: 1m } }
interfaces: {}
layers: !include b.yaml
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("b.yaml"),
        r#"!include a.yaml
"#,
    )
    .unwrap();
    let err = load(dir.path().join("a.yaml")).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("cycle"), "expected cycle error, got {msg}");
}

#[test]
fn unit_roundtrip() {
    use mars_config::units::{parse_bytes, parse_distance_m, parse_duration};
    use std::time::Duration;
    assert_eq!(parse_bytes("12.5KiB").unwrap(), 12_800);
    assert_eq!(parse_bytes("1MiB").unwrap(), 1024 * 1024);
    assert_eq!(parse_bytes("2GiB").unwrap(), 2 * 1024 * 1024 * 1024);
    assert!((parse_distance_m("4096m").unwrap() - 4096.0).abs() < f64::EPSILON);
    assert_eq!(parse_duration("5min").unwrap(), Duration::from_secs(300));
    assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
}

#[test]
fn loads_tiered_sources_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let yaml = r#"
service: { name: t }
sources:
  - id: pg
    type: postgis
    dsn: postgres://example/x
    native_crs: EPSG:25832
artifacts:
  store: { type: fs, path: /tmp/s }
  cache: { path: /tmp/c, max_size: 1MiB }
scales:
  bands:
    - { name: hi, max_denom_exclusive: 25000 }
    - { name: mid, max_denom_exclusive: 250000 }
cells: { grid: regular, origin: [0, 0], size_per_band: { hi: 1024m, mid: 4096m } }
interfaces: {}
layers:
  - name: bygning
    type: polygon
    sources:
      - source: pg
        band: hi
        max_denom_exclusive: 8000
        from: geodanmark_latest.bygning
        geometry_column: geometri
      - source: pg
        band: hi
        max_denom_exclusive: 10000
        from: simplified.bygning_1meter
        geometry_column: geometri
      - source: pg
        band: hi
        max_denom_exclusive: 25000
        from: simplified.bygning_2meter
        geometry_column: geometri
    classes: []
"#;
    let p = dir.path().join("tiered.yaml");
    fs::write(&p, yaml).unwrap();
    let mut cfg = load(&p).expect("load tiered");
    assert_eq!(cfg.layers.len(), 1);
    assert_eq!(cfg.layers[0].sources.len(), 3);
    validate(&mut cfg, dir.path()).expect("validate tiered");

    let s0 = cfg.layers[0].sources[0].scale.as_ref().unwrap();
    let s1 = cfg.layers[0].sources[1].scale.as_ref().unwrap();
    let s2 = cfg.layers[0].sources[2].scale.as_ref().unwrap();
    assert_eq!(s0.min, None);
    assert_eq!(s0.max, Some(8_000));
    assert_eq!(s1.min, Some(8_000));
    assert_eq!(s1.max, Some(10_000));
    assert_eq!(s2.min, Some(10_000));
    assert_eq!(s2.max, Some(25_000));
}

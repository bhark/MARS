#![allow(clippy::unwrap_used)]

use super::*;

fn cfg_with_two_layers() -> Config {
    let yaml = r#"
service: { name: t, title: "T", abstract: "A", contact_email: ops@x }
sources:
  - { id: default, type: postgis, dsn: "postgres://x", native_crs: EPSG:25832 }
artifacts:
  store: { type: fs, path: /tmp }
  cache: { path: /tmp/c, max_size: 1GiB }
scales:
  bands: [{ name: hi, max_denom_exclusive: 25000 }]
cells:
  grid: regular
  origin: [0, 0]
  size_per_band: { hi: 1024m }
interfaces:
  wmts:
    enabled: true
    tile_matrix_sets: [dk_25832]
tile_matrix_sets:
  dk_25832:
    crs: EPSG:25832
    top_left: [120000, 6500000]
    tile_size: [256, 256]
    levels:
      - { id: 0, scale_denominator: 25000000, matrix_width: 1, matrix_height: 1 }
reprojection:
  allowlist: [EPSG:25832]
layers:
  - name: a
    title: "A layer"
    type: polygon
    sources:
      - { from: t, geometry_column: g }
    ows:
      request_gating: { wmts_get_tile: false }
  - name: b
    title: "B layer"
    type: polygon
    sources:
      - { from: t, geometry_column: g }
"#;
    serde_yaml_ng::from_str(yaml).unwrap()
}

#[test]
fn from_config_populates_layer_policies() {
    let cfg = cfg_with_two_layers();
    let wcfg = WmtsConfig::from_config(&cfg);
    let pa = wcfg.layer_policies.get(&LayerId::new("a")).unwrap();
    let pb = wcfg.layer_policies.get(&LayerId::new("b")).unwrap();
    assert!(!pa.get_tile);
    assert!(pa.get_capabilities);
    assert!(pb.get_tile);
    assert!(pb.get_capabilities);
}

#[test]
fn permits_returns_true_for_unknown_layer() {
    let cfg = cfg_with_two_layers();
    let wcfg = WmtsConfig::from_config(&cfg);
    let unknown = LayerId::new("does-not-exist");
    assert!(wcfg.permits(&unknown, ServiceOp::WmtsGetTile));
    assert!(wcfg.permits(&unknown, ServiceOp::WmtsGetCapabilities));
}

#[test]
fn permits_reflects_gating() {
    let cfg = cfg_with_two_layers();
    let wcfg = WmtsConfig::from_config(&cfg);
    assert!(!wcfg.permits(&LayerId::new("a"), ServiceOp::WmtsGetTile));
    assert!(wcfg.permits(&LayerId::new("b"), ServiceOp::WmtsGetTile));
}

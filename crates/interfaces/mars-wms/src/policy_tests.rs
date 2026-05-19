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
interfaces: {}
reprojection:
  allowlist: [EPSG:25832]
layers:
  - name: a
    title: "A layer"
    type: polygon
    sources:
      - { kind: postgis_table, from: t, geometry_column: g }
    ows:
      request_gating: { wms_get_map: false }
  - name: b
    title: "B layer"
    type: polygon
    sources:
      - { kind: postgis_table, from: t, geometry_column: g }
    wms:
      enable_get_feature_info: true
"#;
    serde_yaml_ng::from_str(yaml).unwrap()
}

#[test]
fn from_config_populates_layer_policies() {
    let cfg = cfg_with_two_layers();
    let wcfg = WmsConfig::from_config(&cfg);
    let pa = wcfg.layer_policies.get(&LayerId::new("a")).unwrap();
    let pb = wcfg.layer_policies.get(&LayerId::new("b")).unwrap();
    assert!(!pa.get_map);
    assert!(pa.get_capabilities);
    assert!(!pa.get_feature_info);
    assert!(pa.get_legend_graphic);
    assert!(pb.get_map);
    assert!(pb.get_feature_info);
}

#[test]
fn permits_returns_true_for_unknown_layer() {
    let cfg = cfg_with_two_layers();
    let wcfg = WmsConfig::from_config(&cfg);
    let unknown = LayerId::new("does-not-exist");
    assert!(wcfg.permits(&unknown, ServiceOp::WmsGetMap));
    assert!(wcfg.permits(&unknown, ServiceOp::WmsGetFeatureInfo));
}

#[test]
fn permits_reflects_gating() {
    let cfg = cfg_with_two_layers();
    let wcfg = WmsConfig::from_config(&cfg);
    assert!(!wcfg.permits(&LayerId::new("a"), ServiceOp::WmsGetMap));
    assert!(wcfg.permits(&LayerId::new("b"), ServiceOp::WmsGetMap));
}

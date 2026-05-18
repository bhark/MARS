#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use kube::CustomResourceExt;
use kube::core::ObjectMeta;

use super::{ClusterDefaults, MarsServiceCluster, MarsServiceClusterSpec};

const SAMPLE_YAML: &str = r#"
sourcesCatalog:
  - id: kf_postgis
    nativeCrs: "EPSG:25832"
    type: postgis
    dsn: "postgresql://catalog/db"
  - id: ogr
    nativeCrs: "EPSG:25832"
    type: vectorfile
    cacheDir: /var/cache/mars/vectorfile
artifactStore:
  store: { type: s3, bucket: tiles, endpoint: "http://minio" }
  cache: { path: /var/cache/mars/artifacts, maxSize: 50GiB }
reprojection:
  allowlist: ["EPSG:25832", "EPSG:3857"]
observability:
  logLevel: info
defaults:
  compiler:
    window: 5min
  render:
    jpegQuality: 90
"#;

#[test]
fn spec_roundtrips_through_yaml() {
    let spec: MarsServiceClusterSpec = serde_yaml_ng::from_str(SAMPLE_YAML).expect("parse sample yaml");
    assert_eq!(spec.sources_catalog.len(), 2);
    assert!(spec.reprojection.is_some());
    assert!(spec.defaults.compiler.is_some());
    assert!(spec.defaults.render.is_some());

    let dumped = serde_yaml_ng::to_string(&spec).expect("serialise");
    let reparsed: MarsServiceClusterSpec = serde_yaml_ng::from_str(&dumped).expect("re-parse");

    // structural equality via canonical json
    let a = serde_json::to_value(&spec).expect("to_value a");
    let b = serde_json::to_value(&reparsed).expect("to_value b");
    assert_eq!(a, b);
}

#[test]
fn default_spec_serialises_without_required_payloads() {
    // artifactStore is a required field; explicit empty object suffices for default.
    let spec = MarsServiceClusterSpec {
        artifact_store: serde_json::json!({}),
        ..MarsServiceClusterSpec::default()
    };
    let yaml = serde_yaml_ng::to_string(&spec).expect("serialise");
    let reparsed: MarsServiceClusterSpec = serde_yaml_ng::from_str(&yaml).expect("re-parse");
    assert!(reparsed.sources_catalog.is_empty());
    assert!(reparsed.reprojection.is_none());
    assert!(reparsed.observability.is_none());
    assert!(matches!(
        reparsed.defaults,
        ClusterDefaults {
            compiler: None,
            render: None,
        }
    ));
}

#[test]
fn cr_is_cluster_scoped_with_expected_kube_metadata() {
    let crd = MarsServiceCluster::crd();
    assert_eq!(crd.spec.group, "mars.forn.dk");
    assert_eq!(crd.spec.scope, "Cluster");
    assert_eq!(crd.spec.names.kind, "MarsServiceCluster");
    assert_eq!(crd.spec.names.plural, "marsserviceclusters");
}

#[test]
fn building_a_cr_round_trips_through_yaml() {
    let spec: MarsServiceClusterSpec = serde_yaml_ng::from_str(SAMPLE_YAML).expect("parse sample yaml");
    let cr = MarsServiceCluster {
        metadata: ObjectMeta {
            name: Some("prod-eu".into()),
            ..ObjectMeta::default()
        },
        spec,
    };
    let yaml = serde_yaml_ng::to_string(&cr).expect("serialise");
    let reparsed: MarsServiceCluster = serde_yaml_ng::from_str(&yaml).expect("re-parse");
    assert_eq!(reparsed.metadata.name.as_deref(), Some("prod-eu"));
    assert_eq!(reparsed.spec.sources_catalog.len(), 2);
}

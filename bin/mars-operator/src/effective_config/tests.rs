#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;
use kube::core::ObjectMeta;
use mars_definition_source::DefinitionSourceError;
use mars_test_support::definition_source::FakeDefinitionSource;
use serde_json::json;

use super::*;
use crate::crd::cluster::{ClusterDefaults, MarsServiceCluster, MarsServiceClusterSpec};
use crate::crd::definition::{ClusterRef, DefinitionSpec};
use crate::crd::spec::{MarsService, MarsServiceSpec};

fn cluster() -> MarsServiceCluster {
    MarsServiceCluster {
        metadata: ObjectMeta {
            name: Some("prod-eu".into()),
            ..ObjectMeta::default()
        },
        spec: MarsServiceClusterSpec {
            sources_catalog: vec![json!({
                "id": "kf_postgis",
                "native_crs": "EPSG:25832",
                "type": "postgis",
                "dsn": "postgresql://catalog/db"
            })],
            artifact_store: json!({
                "store": { "type": "fs", "path": "/var/lib/mars/artifacts" },
                "cache": { "path": "/var/cache/mars/artifacts", "max_size": "1GiB" }
            }),
            reprojection: None,
            observability: None,
            defaults: ClusterDefaults::default(),
        },
    }
}

fn svc() -> MarsService {
    MarsService {
        metadata: ObjectMeta {
            name: Some("dagi".into()),
            namespace: Some("gis".into()),
            ..ObjectMeta::default()
        },
        spec: MarsServiceSpec {
            cluster_ref: Some(ClusterRef { name: "prod-eu".into() }),
            definition: Some(DefinitionSpec {
                inline: Some("ignored: used via FakeDefinitionSource".into()),
                ..DefinitionSpec::default()
            }),
            sources: Some(vec!["kf_postgis".into()]),
            ..MarsServiceSpec::default()
        },
        status: None,
    }
}

const DEFINITION_YAML: &str = r#"
service: { name: dagi }
scales:
  bands:
    - name: hi
      max_denom_exclusive: 25000
layers:
  - name: admin
    type: polygon
    sources:
      - source: kf_postgis
        kind: postgis_table
        from: public.admin
        geometry_column: geom
        band: hi
"#;

#[tokio::test]
async fn happy_path_composes_round_trippable_config() {
    let cr = svc();
    let cluster = cluster();
    let spec = cr.spec.definition.clone().unwrap();
    let fake = FakeDefinitionSource::new(Bytes::from(DEFINITION_YAML), "rev-1");

    let out = compose_from_source(&cr, &cluster, &spec, &fake)
        .await
        .expect("compose ok");

    assert_eq!(out.definition.adapter, "inline");
    assert_eq!(out.definition.revision, "rev-1");
    // composed Config round-trips through the canonical YAML pipeline configmap
    // uses; this is what the runtime / compiler pods will actually load.
    let yaml = crate::config::canonicalize_yaml(&out.config).expect("canonicalise");
    let _: mars_config::Config = serde_yaml_ng::from_str(&yaml).expect("re-parse composed config");
}

#[tokio::test]
async fn fetch_error_surfaces_as_typed_error() {
    let cr = svc();
    let cluster = cluster();
    let spec = cr.spec.definition.clone().unwrap();
    let fake = FakeDefinitionSource::new(Bytes::from(DEFINITION_YAML), "rev-1");
    fake.fail_next_fetch(DefinitionSourceError::NotFound { what: "missing".into() });

    let err = compose_from_source(&cr, &cluster, &spec, &fake)
        .await
        .expect_err("must fail");
    assert!(matches!(err, OperatorError::DefinitionFetch(_)), "{err:?}");
}

#[tokio::test]
async fn non_utf8_payload_surfaces_decode_error() {
    let cr = svc();
    let cluster = cluster();
    let spec = cr.spec.definition.clone().unwrap();
    let fake = FakeDefinitionSource::new(Bytes::from_static(&[0xff, 0xfe, 0xfd]), "rev-1");

    let err = compose_from_source(&cr, &cluster, &spec, &fake)
        .await
        .expect_err("must fail");
    assert!(matches!(err, OperatorError::DefinitionDecode(_)), "{err:?}");
}

#[tokio::test]
async fn malformed_yaml_surfaces_decode_error() {
    let cr = svc();
    let cluster = cluster();
    let spec = cr.spec.definition.clone().unwrap();
    let fake = FakeDefinitionSource::new(Bytes::from_static(b"@@@ not yaml"), "rev-1");

    let err = compose_from_source(&cr, &cluster, &spec, &fake)
        .await
        .expect_err("must fail");
    assert!(matches!(err, OperatorError::DefinitionDecode(_)), "{err:?}");
}

#[tokio::test]
async fn unknown_source_id_surfaces_compose_error() {
    let mut cr = svc();
    cr.spec.sources = Some(vec!["does_not_exist".into()]);
    let cluster = cluster();
    let spec = cr.spec.definition.clone().unwrap();
    let fake = FakeDefinitionSource::new(Bytes::from(DEFINITION_YAML), "rev-1");

    let err = compose_from_source(&cr, &cluster, &spec, &fake)
        .await
        .expect_err("must fail");
    assert!(matches!(err, OperatorError::Compose(_)), "{err:?}");
}

#[test]
fn legacy_returns_spec_config_verbatim() {
    let mut cr = svc();
    cr.spec.cluster_ref = None;
    cr.spec.definition = None;
    cr.spec.sources = None;
    cr.spec.config = Some(json!({"service": {"name": "demo"}}));

    let value = legacy(&cr).expect("legacy ok");
    assert_eq!(value, json!({"service": {"name": "demo"}}));
}

#[test]
fn legacy_missing_field_when_spec_config_absent() {
    let cr = svc();
    let err = legacy(&cr).expect_err("missing");
    assert!(matches!(err, OperatorError::MissingField(_)), "{err:?}");
}

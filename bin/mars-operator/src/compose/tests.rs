#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use kube::core::ObjectMeta;
use mars_config::{RenderDefinition, Scales, ServiceMeta, SourceBackend};
use serde_json::json;

use super::*;
use crate::crd::cluster::{ClusterDefaults, MarsServiceCluster, MarsServiceClusterSpec};
use crate::crd::definition::{ConfigMapKeyRef, DefinitionSpec};
use crate::crd::spec::{MarsService, MarsServiceSpec};

fn cluster_with(spec: MarsServiceClusterSpec) -> MarsServiceCluster {
    MarsServiceCluster {
        metadata: ObjectMeta {
            name: Some("prod-eu".into()),
            ..ObjectMeta::default()
        },
        spec,
    }
}

fn minimal_cluster() -> MarsServiceCluster {
    cluster_with(MarsServiceClusterSpec {
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
        reprojection: Some(json!({ "allowlist": ["EPSG:25832", "EPSG:3857"] })),
        observability: Some(json!({ "log_level": "info" })),
        defaults: ClusterDefaults::default(),
    })
}

fn svc_with(sources: Vec<String>, reprojection: Option<serde_json::Value>) -> MarsService {
    MarsService {
        metadata: ObjectMeta {
            name: Some("dagi".into()),
            namespace: Some("gis".into()),
            ..ObjectMeta::default()
        },
        spec: MarsServiceSpec {
            cluster_ref: crate::crd::definition::ClusterRef { name: "prod-eu".into() },
            definition: DefinitionSpec {
                config_map_ref: Some(ConfigMapKeyRef {
                    name: "dagi-definition".into(),
                    key: "definition.yaml".into(),
                }),
                ..DefinitionSpec::default()
            },
            sources,
            reprojection,
            ..MarsServiceSpec::default()
        },
        status: None,
    }
}

fn minimal_def() -> RenderDefinition {
    RenderDefinition {
        service: ServiceMeta {
            name: "dagi".into(),
            ..ServiceMeta::default()
        },
        scales: Scales {
            bands: vec![mars_config::Band {
                name: "hi".into(),
                max_denom: 25_000,
            }],
        },
        interfaces: Default::default(),
        tile_matrix_sets: Default::default(),
        reprojection: Default::default(),
        styles: Default::default(),
        layers: vec![],
    }
}

/// Build a render definition with one layer binding referencing `source_id`.
/// Parses from YAML to avoid pulling `mars-types` as a direct dependency of
/// the operator just to construct test fixtures.
fn def_with_layer_referencing(source_id: &str) -> RenderDefinition {
    let yaml = format!(
        r#"
service: {{ name: dagi }}
scales:
  bands:
    - name: hi
      max_denom_exclusive: 25000
layers:
  - name: admin
    type: polygon
    sources:
      - source: {source_id}
        kind: postgis_table
        from: public.admin
        geometry_column: geom
        band: hi
"#
    );
    RenderDefinition::from_yaml(&yaml).expect("parse def yaml")
}

#[test]
fn happy_path_composes_a_round_trippable_config() {
    let svc = svc_with(vec!["kf_postgis".into()], None);
    let cluster = minimal_cluster();
    let def = def_with_layer_referencing("kf_postgis");

    let cfg = compose_config(&svc, &cluster, def).expect("compose ok");

    assert_eq!(cfg.service.name, "dagi");
    assert_eq!(cfg.sources.len(), 1);
    assert_eq!(cfg.sources[0].id.as_str(), "kf_postgis");
    assert!(matches!(cfg.sources[0].backend, SourceBackend::Postgis(_)));
    assert_eq!(cfg.artifacts.store.kind, "fs");
    // cluster reprojection used when service does not narrow
    assert_eq!(
        cfg.reprojection
            .allowlist
            .iter()
            .map(|c| c.as_str())
            .collect::<Vec<_>>(),
        vec!["EPSG:25832", "EPSG:3857"]
    );
    assert_eq!(cfg.layers.len(), 1);

    // round-trip the composed Config through YAML to ensure no field is half-baked
    let yaml = serde_yaml_ng::to_string(&cfg).expect("serialise");
    let _reparsed: mars_config::Config = serde_yaml_ng::from_str(&yaml).expect("re-parse");
}

#[test]
fn unknown_source_id_in_spec_is_typed_error() {
    let svc = svc_with(vec!["does_not_exist".into()], None);
    let cluster = minimal_cluster();
    let err = compose_config(&svc, &cluster, minimal_def()).expect_err("unknown");
    match err {
        ComposeError::UnknownSourceId { id, known } => {
            assert_eq!(id, "does_not_exist");
            assert!(known.contains("kf_postgis"));
        }
        other => panic!("expected UnknownSourceId, got {other:?}"),
    }
}

#[test]
fn empty_sources_with_layer_refs_is_typed_error() {
    let svc = svc_with(vec![], None);
    let cluster = minimal_cluster();
    let def = def_with_layer_referencing("kf_postgis");
    let err = compose_config(&svc, &cluster, def).expect_err("empty + layers");
    assert!(
        matches!(err, ComposeError::EmptySourcesWithLayerRefs { count } if count == 1),
        "{err:?}"
    );
}

#[test]
fn empty_sources_with_no_layer_refs_composes_ok() {
    let svc = svc_with(vec![], None);
    let cluster = minimal_cluster();
    let cfg = compose_config(&svc, &cluster, minimal_def()).expect("ok");
    assert!(cfg.sources.is_empty());
}

#[test]
fn reprojection_intersects_service_into_cluster() {
    let svc = svc_with(vec!["kf_postgis".into()], Some(json!({ "allowlist": ["EPSG:25832"] })));
    let cluster = minimal_cluster();
    let cfg = compose_config(&svc, &cluster, minimal_def()).expect("ok");
    let codes: Vec<&str> = cfg.reprojection.allowlist.iter().map(|c| c.as_str()).collect();
    assert_eq!(codes, vec!["EPSG:25832"]);
}

#[test]
fn empty_service_reprojection_falls_back_to_cluster() {
    let svc = svc_with(vec!["kf_postgis".into()], Some(json!({ "allowlist": [] })));
    let cluster = minimal_cluster();
    let cfg = compose_config(&svc, &cluster, minimal_def()).expect("ok");
    let codes: Vec<&str> = cfg.reprojection.allowlist.iter().map(|c| c.as_str()).collect();
    assert_eq!(codes, vec!["EPSG:25832", "EPSG:3857"]);
}

#[test]
fn catalog_entry_missing_id_is_typed_error() {
    let cluster = cluster_with(MarsServiceClusterSpec {
        sources_catalog: vec![json!({ "native_crs": "EPSG:25832", "type": "postgis", "dsn": "x" })],
        artifact_store: json!({
            "store": { "type": "fs", "path": "/x" },
            "cache": { "path": "/y", "max_size": "1GiB" }
        }),
        reprojection: None,
        observability: None,
        defaults: ClusterDefaults::default(),
    });
    let svc = svc_with(vec!["kf_postgis".into()], None);
    let err = compose_config(&svc, &cluster, minimal_def()).expect_err("missing id");
    assert!(
        matches!(err, ComposeError::CatalogEntryMissingId { index: 0 }),
        "{err:?}"
    );
}

#[test]
fn malformed_artifact_store_surfaces_typed_error() {
    let cluster = cluster_with(MarsServiceClusterSpec {
        sources_catalog: vec![json!({
            "id": "kf_postgis",
            "native_crs": "EPSG:25832",
            "type": "postgis",
            "dsn": "postgresql://x"
        })],
        // missing required `cache.path` / `cache.max_size` -> deserialise fail
        artifact_store: json!({ "store": { "type": "fs" }, "cache": {} }),
        reprojection: None,
        observability: None,
        defaults: ClusterDefaults::default(),
    });
    let svc = svc_with(vec!["kf_postgis".into()], None);
    let err = compose_config(&svc, &cluster, minimal_def()).expect_err("bad artifacts");
    assert!(
        matches!(
            err,
            ComposeError::InvalidClusterField {
                field: "artifactStore",
                target: "Artifacts",
                ..
            }
        ),
        "{err:?}"
    );
}

#[test]
fn malformed_source_catalog_entry_surfaces_typed_error() {
    // 'type' on a postgis source is required by SourceBackend's tag.
    let cluster = cluster_with(MarsServiceClusterSpec {
        sources_catalog: vec![json!({ "id": "kf_postgis", "native_crs": "EPSG:25832" })],
        artifact_store: json!({
            "store": { "type": "fs", "path": "/x" },
            "cache": { "path": "/y", "max_size": "1GiB" }
        }),
        reprojection: None,
        observability: None,
        defaults: ClusterDefaults::default(),
    });
    let svc = svc_with(vec!["kf_postgis".into()], None);
    let err = compose_config(&svc, &cluster, minimal_def()).expect_err("bad src");
    assert!(
        matches!(err, ComposeError::InvalidCatalogEntry { index: 0, .. }),
        "{err:?}"
    );
}

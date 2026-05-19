#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use kube::CustomResourceExt;

use super::*;
use crate::crd::cluster::MarsServiceCluster;
use crate::crd::definition::{ConfigMapKeyRef, DefinitionSpec, GitRef, GitRevision, S3Ref, SecretRef};

const NEW_SHAPE_YAML: &str = r#"
compiler:
  replicas: 1
runtime:
  replicas: 2
clusterRef:
  name: prod-eu
definition:
  configMapRef:
    name: dagi-definition
    key: definition.yaml
sources: ["kf_postgis"]
reprojection:
  allowlist: ["EPSG:25832"]
"#;

const LEGACY_SHAPE_YAML: &str = r#"
compiler:
  replicas: 1
runtime:
  replicas: 2
config:
  service:
    name: demo
  sources:
    - id: default
      kind: stub
  artifacts:
    store:
      type: fs
      path: /var/lib/mars/artifacts
"#;

fn new_shape_spec() -> MarsServiceSpec {
    MarsServiceSpec {
        cluster_ref: Some(ClusterRef { name: "prod-eu".into() }),
        definition: Some(DefinitionSpec {
            config_map_ref: Some(ConfigMapKeyRef {
                name: "dagi-definition".into(),
                key: "definition.yaml".into(),
            }),
            ..DefinitionSpec::default()
        }),
        sources: Some(vec!["kf_postgis".into()]),
        ..MarsServiceSpec::default()
    }
}

fn legacy_spec() -> MarsServiceSpec {
    MarsServiceSpec {
        config: Some(serde_json::json!({"service": {"name": "demo"}})),
        ..MarsServiceSpec::default()
    }
}

#[test]
fn new_shape_round_trips_through_yaml() {
    let spec: MarsServiceSpec = serde_yaml_ng::from_str(NEW_SHAPE_YAML).expect("parse new shape");
    assert!(spec.config.is_none());
    assert!(spec.cluster_ref.is_some());
    assert!(spec.definition.is_some());
    assert_eq!(spec.sources.as_deref(), Some(["kf_postgis".to_string()].as_slice()));
    assert!(spec.reprojection.is_some());

    let yaml = serde_yaml_ng::to_string(&spec).expect("serialise");
    let reparsed: MarsServiceSpec = serde_yaml_ng::from_str(&yaml).expect("re-parse");
    let a = serde_json::to_value(&spec).expect("a");
    let b = serde_json::to_value(&reparsed).expect("b");
    assert_eq!(a, b);
}

#[test]
fn legacy_shape_round_trips_through_yaml() {
    let spec: MarsServiceSpec = serde_yaml_ng::from_str(LEGACY_SHAPE_YAML).expect("parse legacy");
    assert!(spec.config.is_some());
    assert!(spec.cluster_ref.is_none());
    assert!(spec.definition.is_none());
    assert!(spec.sources.is_none());

    let yaml = serde_yaml_ng::to_string(&spec).expect("serialise");
    let reparsed: MarsServiceSpec = serde_yaml_ng::from_str(&yaml).expect("re-parse");
    let a = serde_json::to_value(&spec).expect("a");
    let b = serde_json::to_value(&reparsed).expect("b");
    assert_eq!(a, b);
}

#[test]
fn validate_spec_accepts_legacy_shape() {
    validate_spec(&legacy_spec()).expect("legacy shape is valid");
}

#[test]
fn validate_spec_accepts_new_shape_with_each_definition_variant() {
    let inline = MarsServiceSpec {
        definition: Some(DefinitionSpec {
            inline: Some("service: { name: x }".into()),
            ..DefinitionSpec::default()
        }),
        ..new_shape_spec()
    };
    validate_spec(&inline).expect("inline variant");

    let cm = new_shape_spec();
    validate_spec(&cm).expect("configMapRef variant");

    let git = MarsServiceSpec {
        definition: Some(DefinitionSpec {
            git_ref: Some(GitRef {
                url: "https://example.com/foo.git".into(),
                git_ref: GitRevision {
                    branch: Some("main".into()),
                    ..GitRevision::default()
                },
                path: "definition.yaml".into(),
                interval: None,
                secret_ref: Some(SecretRef {
                    name: "git-creds".into(),
                }),
            }),
            ..DefinitionSpec::default()
        }),
        ..new_shape_spec()
    };
    validate_spec(&git).expect("gitRef variant");

    let s3 = MarsServiceSpec {
        definition: Some(DefinitionSpec {
            s3_ref: Some(S3Ref {
                endpoint: None,
                region: "us-east-1".into(),
                bucket: "defs".into(),
                key: "dagi.yaml".into(),
                interval: None,
                secret_ref: None,
            }),
            ..DefinitionSpec::default()
        }),
        ..new_shape_spec()
    };
    validate_spec(&s3).expect("s3Ref variant");
}

#[test]
fn validate_spec_rejects_both_shapes_set() {
    let spec = MarsServiceSpec {
        config: Some(serde_json::json!({})),
        ..new_shape_spec()
    };
    let err = validate_spec(&spec).expect_err("both shapes");
    assert!(matches!(err, SpecValidationError::BothShapes));
}

#[test]
fn validate_spec_rejects_neither_shape_set() {
    let spec = MarsServiceSpec::default();
    let err = validate_spec(&spec).expect_err("neither shape");
    assert!(matches!(err, SpecValidationError::NeitherShape));
}

#[test]
fn validate_spec_rejects_partial_new_shape() {
    let spec = MarsServiceSpec {
        cluster_ref: Some(ClusterRef { name: "prod-eu".into() }),
        ..MarsServiceSpec::default()
    };
    let err = validate_spec(&spec).expect_err("missing definition");
    assert!(
        matches!(err, SpecValidationError::NewShapeMissing(f) if f == "definition"),
        "{err:?}"
    );

    let spec = MarsServiceSpec {
        definition: Some(DefinitionSpec {
            inline: Some("x".into()),
            ..DefinitionSpec::default()
        }),
        sources: Some(vec!["a".into()]),
        ..MarsServiceSpec::default()
    };
    let err = validate_spec(&spec).expect_err("missing clusterRef");
    assert!(
        matches!(err, SpecValidationError::NewShapeMissing(f) if f == "clusterRef"),
        "{err:?}"
    );

    let spec = MarsServiceSpec {
        cluster_ref: Some(ClusterRef { name: "prod-eu".into() }),
        definition: Some(DefinitionSpec {
            inline: Some("x".into()),
            ..DefinitionSpec::default()
        }),
        ..MarsServiceSpec::default()
    };
    let err = validate_spec(&spec).expect_err("missing sources");
    assert!(
        matches!(err, SpecValidationError::NewShapeMissing(f) if f == "sources"),
        "{err:?}"
    );
}

#[test]
fn validate_spec_rejects_definition_with_zero_variants() {
    let spec = MarsServiceSpec {
        definition: Some(DefinitionSpec::default()),
        ..new_shape_spec()
    };
    let err = validate_spec(&spec).expect_err("zero variants");
    assert!(matches!(err, SpecValidationError::DefinitionVariantCount(0)));
}

#[test]
fn validate_spec_rejects_bootstrap_on_new_path() {
    let spec = MarsServiceSpec {
        bootstrap: Some(crate::crd::bootstrap::BootstrapSpec {
            enabled: true,
            admin_secret_ref: Some(crate::crd::bootstrap::SecretKeyRef {
                name: "admin".into(),
                key: "dsn".into(),
            }),
            admin_credentials_ref: None,
            runtime_password_secret_ref: None,
            teardown_on_delete: crate::crd::bootstrap::TeardownPolicy::default(),
        }),
        ..new_shape_spec()
    };
    let err = validate_spec(&spec).expect_err("bootstrap on new path");
    assert!(matches!(err, SpecValidationError::BootstrapOnNewPath), "{err:?}");
}

#[test]
fn print_crd_emits_multi_doc_with_cluster_first() {
    let cluster = serde_yaml_ng::to_string(&MarsServiceCluster::crd()).expect("cluster yaml");
    let service = serde_yaml_ng::to_string(&MarsService::crd()).expect("service yaml");
    let combined = format!("{cluster}---\n{service}");

    let docs: Vec<serde_yaml_ng::Value> = serde_yaml_ng::Deserializer::from_str(&combined)
        .map(serde_yaml_ng::Value::deserialize)
        .collect::<Result<_, _>>()
        .expect("parse multi-doc yaml");
    assert_eq!(docs.len(), 2, "expected two docs");

    let kind_of = |d: &serde_yaml_ng::Value| {
        d.get("spec")
            .and_then(|s| s.get("names"))
            .and_then(|n| n.get("kind"))
            .and_then(|k| k.as_str())
            .map(str::to_owned)
    };
    assert_eq!(kind_of(&docs[0]).as_deref(), Some("MarsServiceCluster"));
    assert_eq!(kind_of(&docs[1]).as_deref(), Some("MarsService"));
}

#[test]
fn crd_marks_spec_config_deprecated_in_schema_description() {
    let crd = MarsService::crd();
    let description = crd
        .spec
        .versions
        .iter()
        .find(|v| v.name == "v1alpha1")
        .and_then(|v| v.schema.as_ref())
        .and_then(|s| s.open_api_v3_schema.as_ref())
        .and_then(|root| root.properties.as_ref()?.get("spec"))
        .and_then(|spec| spec.properties.as_ref()?.get("config"))
        .and_then(|cfg| cfg.description.clone())
        .expect("config field description present");
    assert!(
        description.contains("DEPRECATED"),
        "expected DEPRECATED in spec.config description, got: {description}"
    );
}

#[test]
fn validate_spec_rejects_definition_with_multiple_variants() {
    let spec = MarsServiceSpec {
        definition: Some(DefinitionSpec {
            inline: Some("x".into()),
            config_map_ref: Some(ConfigMapKeyRef {
                name: "a".into(),
                key: "b".into(),
            }),
            ..DefinitionSpec::default()
        }),
        ..new_shape_spec()
    };
    let err = validate_spec(&spec).expect_err("two variants");
    assert!(matches!(err, SpecValidationError::DefinitionVariantCount(2)));
}

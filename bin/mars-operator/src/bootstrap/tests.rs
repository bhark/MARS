#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::children::test_support;
use crate::crd::{BootstrapSpec, TeardownPolicy};

fn admin() -> SecretKeyRef {
    SecretKeyRef {
        name: "admin-secret".into(),
        key: "dsn".into(),
    }
}

fn runtime() -> SecretKeyRef {
    SecretKeyRef {
        name: "runtime-secret".into(),
        key: "password".into(),
    }
}

fn bs_spec() -> BootstrapSpec {
    BootstrapSpec {
        enabled: true,
        admin_secret_ref: Some(admin()),
        admin_credentials_ref: None,
        runtime_password_secret_ref: Some(runtime()),
        teardown_on_delete: TeardownPolicy::default(),
    }
}

fn bs_spec_managed_runtime() -> BootstrapSpec {
    BootstrapSpec {
        enabled: true,
        admin_secret_ref: Some(admin()),
        admin_credentials_ref: None,
        runtime_password_secret_ref: None,
        teardown_on_delete: TeardownPolicy::default(),
    }
}

fn source_bootstrap() -> SourceBootstrap {
    SourceBootstrap {
        role: "mars_replicator".into(),
        publication: "mars_pub".into(),
        slot: "mars_slot".into(),
        schemas: vec!["public".into(), "geo".into()],
    }
}

fn inputs(bs: &BootstrapSpec) -> PlanInputs {
    let admin_dsn_ref = bs.admin_secret_ref.clone().unwrap_or_else(|| SecretKeyRef {
        name: "demo-bootstrap-admin-credentials".into(),
        key: "dsn".into(),
    });
    PlanInputs {
        source_bootstrap: source_bootstrap(),
        runtime_password_ref: bs.runtime_password_secret_ref.clone().unwrap_or_else(|| SecretKeyRef {
            name: "demo-runtime-credentials".into(),
            key: "password".into(),
        }),
        admin_dsn_ref,
        admin_secret_resource_version: "100".into(),
        runtime_secret_resource_version: "200".into(),
    }
}

#[test]
fn plan_hash_is_stable_for_identical_inputs() {
    let bs = bs_spec();
    let h1 = plan_hash(&inputs(&bs));
    let h2 = plan_hash(&inputs(&bs));
    assert_eq!(h1, h2);
    assert_eq!(h1.len(), 10);
}

#[test]
fn plan_hash_changes_on_schema_change() {
    let bs = bs_spec();
    let h1 = plan_hash(&inputs(&bs));
    let mut other = inputs(&bs);
    other.source_bootstrap.schemas = vec!["public".into(), "extra".into()];
    let h2 = plan_hash(&other);
    assert_ne!(h1, h2);
}

#[test]
fn plan_hash_changes_on_admin_secret_rotation() {
    let bs = bs_spec();
    let h1 = plan_hash(&inputs(&bs));
    let mut other = inputs(&bs);
    other.admin_secret_resource_version = "101".into();
    let h2 = plan_hash(&other);
    assert_ne!(h1, h2);
}

#[test]
fn plan_hash_independent_of_schema_order() {
    let bs = bs_spec();
    let h1 = plan_hash(&inputs(&bs));
    let mut other = inputs(&bs);
    other.source_bootstrap.schemas = vec!["geo".into(), "public".into()];
    let h2 = plan_hash(&other);
    assert_eq!(h1, h2);
}

#[test]
fn render_bootstrap_job_uses_resolved_runtime_password_ref() {
    let cr = test_support::cr("demo", "svc-ns");
    let bs = bs_spec_managed_runtime();
    let job = render_bootstrap_job(
        &cr,
        test_support::TEST_IMAGE,
        &inputs(&bs),
        "abcdef0123",
        test_support::owner_ref(),
    )
    .unwrap();
    let pod = test_support::job_pod_spec(&job);
    let runtime_env = test_support::env_var(&pod.containers[0], "MARS_RUNTIME_PASSWORD");
    let sref = runtime_env
        .value_from
        .as_ref()
        .unwrap()
        .secret_key_ref
        .as_ref()
        .unwrap();
    assert_eq!(sref.name, "demo-runtime-credentials");
    assert_eq!(sref.key, "password");
}

#[test]
fn plan_hash_changes_when_runtime_password_ref_changes() {
    let bs = bs_spec();
    let h1 = plan_hash(&inputs(&bs));
    let mut other = inputs(&bs);
    other.runtime_password_ref = SecretKeyRef {
        name: "different".into(),
        key: "different".into(),
    };
    let h2 = plan_hash(&other);
    assert_ne!(h1, h2);
}

#[test]
fn render_bootstrap_job_has_two_secret_env_vars() {
    let cr = test_support::cr("demo", "svc-ns");
    let bs = bs_spec();
    let job = render_bootstrap_job(
        &cr,
        test_support::TEST_IMAGE,
        &inputs(&bs),
        "abcdef0123",
        test_support::owner_ref(),
    )
    .unwrap();
    assert_eq!(job.metadata.name.as_deref(), Some("demo-bootstrap-abcdef0123"));
    let pod = test_support::job_pod_spec(&job);
    let envs = pod.containers[0].env.as_ref().unwrap();
    assert!(envs.iter().any(|e| e.name == "MARS_ADMIN_DSN"));
    assert!(envs.iter().any(|e| e.name == "MARS_RUNTIME_PASSWORD"));
    assert_eq!(pod.restart_policy.as_deref(), Some("Never"));
    assert_eq!(pod.service_account_name.as_deref(), Some("demo-bootstrap"));
}

#[test]
fn render_bootstrap_job_propagates_compiler_env_and_env_from() {
    let mut cr = test_support::cr("demo", "svc-ns");
    cr.spec.compiler.env = vec![
        EnvVarSpec {
            name: "PG_DSN".into(),
            value: Some("postgres://example".into()),
            value_from: None,
        },
        // a name collision must not shadow the projected secret env.
        EnvVarSpec {
            name: "MARS_ADMIN_DSN".into(),
            value: Some("should-be-ignored".into()),
            value_from: None,
        },
    ];
    cr.spec.compiler.env_from = vec![crate::crd::EnvFromSourceSpec {
        prefix: None,
        secret_ref: Some(crate::crd::LocalObjectReferenceSpec {
            name: "mars-s3-credentials".into(),
            optional: None,
        }),
        config_map_ref: None,
    }];
    let bs = bs_spec();
    let job = render_bootstrap_job(
        &cr,
        test_support::TEST_IMAGE,
        &inputs(&bs),
        "abcdef0123",
        test_support::owner_ref(),
    )
    .unwrap();
    let container = job.spec.unwrap().template.spec.unwrap().containers.remove(0);
    let envs = container.env.unwrap();
    // compiler PG_DSN is projected so the mounted mars.yaml resolves.
    let pg = envs.iter().find(|e| e.name == "PG_DSN").unwrap();
    assert_eq!(pg.value.as_deref(), Some("postgres://example"));
    // the projected admin secret wins over a colliding compiler entry.
    let admin: Vec<_> = envs.iter().filter(|e| e.name == "MARS_ADMIN_DSN").collect();
    assert_eq!(admin.len(), 1);
    assert!(admin[0].value.is_none());
    assert!(admin[0].value_from.is_some());
    // env_from is propagated from the compiler spec.
    let env_from = container.env_from.unwrap();
    assert_eq!(env_from.len(), 1);
    assert_eq!(env_from[0].secret_ref.as_ref().unwrap().name, "mars-s3-credentials");
}

#[test]
fn render_teardown_job_omits_drop_flags_when_disabled() {
    let cr = test_support::cr("demo", "svc-ns");
    let policy = TeardownPolicy {
        slot: true,
        publication: false,
        role: false,
    };
    let dsn_ref = admin();
    let job = render_teardown_job(
        &cr,
        test_support::TEST_IMAGE,
        &dsn_ref,
        &policy,
        test_support::owner_ref(),
    )
    .unwrap();
    let pod = test_support::job_pod_spec(&job);
    let args = pod.containers[0].args.as_ref().unwrap();
    assert!(args.iter().any(|a| a == "--drop-slot"));
    assert!(!args.iter().any(|a| a == "--drop-publication"));
    assert!(!args.iter().any(|a| a == "--drop-role"));
}

#[test]
fn render_teardown_job_omits_runtime_password_env() {
    let cr = test_support::cr("demo", "svc-ns");
    let dsn_ref = admin();
    let job = render_teardown_job(
        &cr,
        test_support::TEST_IMAGE,
        &dsn_ref,
        &TeardownPolicy::default(),
        test_support::owner_ref(),
    )
    .unwrap();
    let pod = test_support::job_pod_spec(&job);
    let envs = pod.containers[0].env.as_ref().unwrap();
    assert!(envs.iter().any(|e| e.name == "MARS_ADMIN_DSN"));
    assert!(!envs.iter().any(|e| e.name == "MARS_RUNTIME_PASSWORD"));
}

#[test]
fn render_bootstrap_job_with_managed_admin_credentials_uses_secret_key_ref() {
    let cr = test_support::cr("demo", "svc-ns");
    let bs = bs_spec_managed_runtime();
    let mut input = inputs(&bs);
    // simulate the component-style admin-credentials branch, which has
    // already materialised the composed DSN into <svc>-bootstrap-admin-credentials.
    input.admin_dsn_ref = SecretKeyRef {
        name: "demo-bootstrap-admin-credentials".into(),
        key: "dsn".into(),
    };
    let job = render_bootstrap_job(
        &cr,
        test_support::TEST_IMAGE,
        &input,
        "abcdef0123",
        test_support::owner_ref(),
    )
    .unwrap();
    let pod = test_support::job_pod_spec(&job);
    let admin_env = test_support::env_var(&pod.containers[0], "MARS_ADMIN_DSN");
    assert!(admin_env.value.is_none());
    let sref = admin_env.value_from.as_ref().unwrap().secret_key_ref.as_ref().unwrap();
    assert_eq!(sref.name, "demo-bootstrap-admin-credentials");
    assert_eq!(sref.key, "dsn");
}

#[test]
fn plan_hash_changes_when_admin_dsn_ref_changes() {
    let bs = bs_spec();
    let h1 = plan_hash(&inputs(&bs));
    let mut other = inputs(&bs);
    other.admin_dsn_ref = SecretKeyRef {
        name: "different-admin-secret".into(),
        key: "different-key".into(),
    };
    let h2 = plan_hash(&other);
    assert_ne!(h1, h2);
}

#[test]
fn extract_source_bootstrap_returns_none_without_bootstrap_block() {
    let config = serde_json::json!({
        "sources": [{ "id": "default", "change_feed": { "publication": "p", "slot": "s" } }]
    });
    assert!(extract_source_bootstrap(&config).is_none());
}

#[test]
fn extract_source_bootstrap_pulls_names_and_schemas() {
    let config = serde_json::json!({
        "sources": [{
            "id": "default",
            "change_feed": { "publication": "mars_pub", "slot": "mars_slot" },
            "bootstrap": { "role": "mars_replicator", "schemas": ["a", "b"] }
        }]
    });
    let bs = extract_source_bootstrap(&config).unwrap();
    assert_eq!(bs.role, "mars_replicator");
    assert_eq!(bs.publication, "mars_pub");
    assert_eq!(bs.slot, "mars_slot");
    assert_eq!(bs.schemas, vec!["a".to_string(), "b".to_string()]);
}

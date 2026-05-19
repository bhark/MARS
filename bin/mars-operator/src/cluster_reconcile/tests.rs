#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use kube::core::ObjectMeta;
use mars_config::SourceBackend;
use serde_json::json;

use super::jobs::synthesise_config;
use super::plan::{CatalogBootstrapPlan, cluster_bootstrap_job_name, plan_hash, plan_jobs};
use super::{SecretKeyRef, owner_reference};
use crate::crd::cluster::{ClusterDefaults, MarsServiceCluster, MarsServiceClusterSpec};

fn cluster(name: &str, catalog: Vec<serde_json::Value>) -> MarsServiceCluster {
    MarsServiceCluster {
        metadata: ObjectMeta {
            name: Some(name.into()),
            uid: Some("00000000-0000-0000-0000-000000000099".into()),
            ..ObjectMeta::default()
        },
        spec: MarsServiceClusterSpec {
            sources_catalog: catalog,
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

fn postgis_source_with_bootstrap(id: &str) -> serde_json::Value {
    json!({
        "id": id,
        "native_crs": "EPSG:25832",
        "type": "postgis",
        "dsn": "postgresql://catalog/db",
        "change_feed": {
            "type": "pgoutput",
            "publication": "mars_pub",
            "slot": "mars_slot"
        },
        "bootstrap": {
            "enabled": true,
            "adminSecretRef": { "name": "admin", "key": "dsn" },
            "role": "mars_replicator",
            "schemas": ["public", "geo"]
        }
    })
}

fn postgis_source_no_bootstrap(id: &str) -> serde_json::Value {
    json!({
        "id": id,
        "native_crs": "EPSG:25832",
        "type": "postgis",
        "dsn": "postgresql://catalog/db"
    })
}

#[test]
fn plan_jobs_picks_only_entries_with_bootstrap() {
    let cr = cluster(
        "prod-eu",
        vec![
            postgis_source_with_bootstrap("kf_postgis"),
            postgis_source_no_bootstrap("ogr_pg"),
        ],
    );
    let plans = plan_jobs(&cr).expect("plan ok");
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].source_id, "kf_postgis");
    assert_eq!(plans[0].cluster_name, "prod-eu");
}

#[test]
fn plan_jobs_returns_empty_when_no_bootstrap_configured() {
    let cr = cluster(
        "prod-eu",
        vec![postgis_source_no_bootstrap("a"), postgis_source_no_bootstrap("b")],
    );
    let plans = plan_jobs(&cr).expect("plan ok");
    assert!(plans.is_empty());
}

#[test]
fn plan_jobs_skips_non_postgis_entries_with_bootstrap() {
    let mut entry = json!({
        "id": "ogr",
        "native_crs": "EPSG:25832",
        "type": "vectorfile",
        "cache_dir": "/var/cache/mars/vectorfile"
    });
    // splice a bootstrap block onto a vectorfile entry: not supported
    entry["bootstrap"] = json!({ "role": "r", "schemas": ["public"] });
    let cr = cluster("prod-eu", vec![entry]);
    let plans = plan_jobs(&cr).expect("plan ok");
    assert!(plans.is_empty());
}

#[test]
fn plan_jobs_skips_entries_that_fail_to_deserialise() {
    // missing the `type` tag means SourceBackend can't pick a variant
    let bad = json!({
        "id": "broken",
        "native_crs": "EPSG:25832",
        "bootstrap": { "role": "r", "schemas": ["s"] }
    });
    let cr = cluster("prod-eu", vec![bad]);
    let plans = plan_jobs(&cr).expect("plan ok");
    assert!(plans.is_empty());
}

#[test]
fn plan_jobs_captures_bootstrap_orchestration_knobs() {
    let entry = json!({
        "id": "kf_postgis",
        "native_crs": "EPSG:25832",
        "type": "postgis",
        "dsn": "postgresql://catalog/db",
        "change_feed": {
            "type": "pgoutput",
            "publication": "mars_pub",
            "slot": "mars_slot"
        },
        "bootstrap": {
            "enabled": false,
            "adminCredentialsRef": {
                "secretName": "pg-superuser"
            },
            "runtimePasswordSecretRef": { "name": "rt-pw", "key": "password" },
            "teardownOnDelete": { "slot": false, "publication": false, "role": false },
            "role": "mars_replicator",
            "schemas": ["public"]
        }
    });
    let cr = cluster("prod-eu", vec![entry]);
    let plans = plan_jobs(&cr).expect("plan ok");
    assert_eq!(plans.len(), 1);
    let bs = &plans[0].bootstrap;
    assert!(!bs.enabled);
    assert!(bs.admin_credentials_ref.is_some());
    assert!(bs.runtime_password_secret_ref.is_some());
    assert!(!bs.teardown_on_delete.slot);
}

#[test]
fn job_name_is_deterministic_for_identical_plan_inputs() {
    let plan = sample_plan();
    let admin = SecretKeyRef {
        name: "admin".into(),
        key: "dsn".into(),
    };
    let runtime = SecretKeyRef {
        name: "rt-pw".into(),
        key: "password".into(),
    };
    let h1 = plan_hash(&plan, &admin, "100", &runtime, "200");
    let h2 = plan_hash(&plan, &admin, "100", &runtime, "200");
    assert_eq!(h1, h2);
    assert_eq!(h1.len(), 10);
    assert_eq!(
        cluster_bootstrap_job_name(&plan.cluster_name, &plan.source_id, &h1),
        format!("prod-eu-bootstrap-kf_postgis-{h1}")
    );
}

#[test]
fn plan_hash_rolls_on_schema_change() {
    let plan = sample_plan();
    let admin = SecretKeyRef {
        name: "admin".into(),
        key: "dsn".into(),
    };
    let runtime = SecretKeyRef {
        name: "rt-pw".into(),
        key: "password".into(),
    };
    let h1 = plan_hash(&plan, &admin, "100", &runtime, "200");
    let mut other = plan.clone();
    if let SourceBackend::Postgis(pg) = &mut other.source.backend
        && let Some(bs) = pg.bootstrap.as_mut()
    {
        bs.schemas.push("extra".into());
    }
    let h2 = plan_hash(&other, &admin, "100", &runtime, "200");
    assert_ne!(h1, h2);
}

#[test]
fn plan_hash_independent_of_schema_order() {
    let mut plan = sample_plan();
    if let SourceBackend::Postgis(pg) = &mut plan.source.backend
        && let Some(bs) = pg.bootstrap.as_mut()
    {
        bs.schemas = vec!["a".into(), "b".into()];
    }
    let admin = SecretKeyRef {
        name: "admin".into(),
        key: "dsn".into(),
    };
    let runtime = SecretKeyRef {
        name: "rt-pw".into(),
        key: "password".into(),
    };
    let h1 = plan_hash(&plan, &admin, "100", &runtime, "200");

    if let SourceBackend::Postgis(pg) = &mut plan.source.backend
        && let Some(bs) = pg.bootstrap.as_mut()
    {
        bs.schemas = vec!["b".into(), "a".into()];
    }
    let h2 = plan_hash(&plan, &admin, "100", &runtime, "200");
    assert_eq!(h1, h2);
}

#[test]
fn synthesise_config_validates() {
    let plan = sample_plan();
    let cfg = synthesise_config(&plan).expect("synth ok");
    assert_eq!(cfg["service"]["name"], "prod-eu-kf_postgis-bootstrap");
    assert_eq!(cfg["sources"][0]["id"], "kf_postgis");
}

#[test]
fn owner_reference_carries_cluster_uid() {
    let cr = cluster("prod-eu", vec![]);
    let owner = owner_reference(&cr).expect("owner ok");
    assert_eq!(owner.kind, "MarsServiceCluster");
    assert_eq!(owner.name, "prod-eu");
    assert_eq!(owner.uid, "00000000-0000-0000-0000-000000000099");
    assert_eq!(owner.controller, Some(true));
    assert_eq!(owner.block_owner_deletion, Some(true));
}

fn sample_plan() -> CatalogBootstrapPlan {
    let cr = cluster("prod-eu", vec![postgis_source_with_bootstrap("kf_postgis")]);
    let mut plans = plan_jobs(&cr).expect("plan ok");
    plans.remove(0)
}

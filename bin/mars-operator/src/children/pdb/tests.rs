#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;
use crate::children::test_support;
use crate::crd::PodDisruptionBudgetSpec;

#[test]
fn build_returns_none_when_spec_absent() {
    let cr = test_support::cr("demo", "svc-ns");
    let out = build(&cr, test_support::owner_ref()).unwrap();
    assert!(out.is_none());
}

#[test]
fn build_emits_pdb_with_min_available_integer() {
    let mut cr = test_support::cr("demo", "svc-ns");
    cr.spec.runtime.pod_disruption_budget = Some(PodDisruptionBudgetSpec {
        min_available: Some("2".into()),
        max_unavailable: None,
    });
    let pdb = build(&cr, test_support::owner_ref()).unwrap().unwrap();
    assert_eq!(pdb.metadata.name.as_deref(), Some("demo-runtime"));
    assert_eq!(pdb.metadata.namespace.as_deref(), Some("svc-ns"));
    let spec = pdb.spec.unwrap();
    match spec.min_available.unwrap() {
        IntOrString::Int(n) => assert_eq!(n, 2),
        other => panic!("expected Int, got {other:?}"),
    }
    assert!(spec.max_unavailable.is_none());
}

#[test]
fn build_emits_pdb_with_max_unavailable_percentage() {
    let mut cr = test_support::cr("demo", "svc-ns");
    cr.spec.runtime.pod_disruption_budget = Some(PodDisruptionBudgetSpec {
        min_available: None,
        max_unavailable: Some("50%".into()),
    });
    let pdb = build(&cr, test_support::owner_ref()).unwrap().unwrap();
    let spec = pdb.spec.unwrap();
    match spec.max_unavailable.unwrap() {
        IntOrString::String(s) => assert_eq!(s, "50%"),
        other => panic!("expected String, got {other:?}"),
    }
    assert!(spec.min_available.is_none());
}

#[test]
fn build_selector_matches_runtime_deployment_labels() {
    let mut cr = test_support::cr("demo", "svc-ns");
    cr.spec.runtime.pod_disruption_budget = Some(PodDisruptionBudgetSpec {
        min_available: Some("1".into()),
        max_unavailable: None,
    });
    let pdb = build(&cr, test_support::owner_ref()).unwrap().unwrap();
    let spec = pdb.spec.unwrap();
    let labels = spec.selector.unwrap().match_labels.unwrap();
    assert_eq!(
        labels.get("app.kubernetes.io/component").map(String::as_str),
        Some(COMPONENT_RUNTIME)
    );
    assert_eq!(
        labels.get("app.kubernetes.io/instance").map(String::as_str),
        Some("demo")
    );
}

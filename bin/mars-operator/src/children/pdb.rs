//! Optional PodDisruptionBudget sibling targeting the runtime Deployment.
//! Reuses the runtime label selector so it can never drift from the
//! Deployment's pods.

use k8s_openapi::api::policy::v1::{PodDisruptionBudget, PodDisruptionBudgetSpec as PdbSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use crate::children::labels::{self, COMPONENT_RUNTIME, runtime_pdb_name};
use crate::crd::MarsService;
use crate::error::Result;

pub(crate) fn build(cr: &MarsService, owner_ref: OwnerReference) -> Result<Option<PodDisruptionBudget>> {
    let Some(spec) = cr.spec.runtime.pod_disruption_budget.as_ref() else {
        return Ok(None);
    };
    let svc = cr
        .metadata
        .name
        .clone()
        .ok_or_else(|| crate::error::OperatorError::MissingField("metadata.name".into()))?;
    let ns = cr.metadata.namespace.clone();

    Ok(Some(PodDisruptionBudget {
        metadata: ObjectMeta {
            name: Some(runtime_pdb_name(&svc)),
            namespace: ns,
            labels: Some(labels::labels(&svc, COMPONENT_RUNTIME)),
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        spec: Some(PdbSpec {
            min_available: spec.min_available.as_deref().map(parse_int_or_string),
            max_unavailable: spec.max_unavailable.as_deref().map(parse_int_or_string),
            selector: Some(LabelSelector {
                match_labels: Some(labels::selector(&svc, COMPONENT_RUNTIME)),
                ..Default::default()
            }),
            ..Default::default()
        }),
        status: None,
    }))
}

/// Parse a string as a kube `IntOrString`: pure-digit input becomes `Int`,
/// anything else passes through as `String` (the kube apiserver validates
/// percentage syntax like `"50%"`).
fn parse_int_or_string(s: &str) -> IntOrString {
    match s.parse::<i32>() {
        Ok(n) => IntOrString::Int(n),
        Err(_) => IntOrString::String(s.to_string()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
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
}

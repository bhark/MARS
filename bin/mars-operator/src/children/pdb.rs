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
mod tests;

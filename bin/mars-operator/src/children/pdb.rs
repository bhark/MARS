//! PodDisruptionBudget sibling targeting the runtime Deployment. A
//! multi-replica runtime gets `maxUnavailable: 1` by default; an explicit
//! `runtime.podDisruptionBudget` overrides that. Reuses the runtime label
//! selector so it can never drift from the Deployment's pods.

use k8s_openapi::api::policy::v1::{PodDisruptionBudget, PodDisruptionBudgetSpec as PdbSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use crate::children::labels::{self, COMPONENT_RUNTIME, runtime_pdb_name};
use crate::crd::spec::MarsService;
use crate::error::Result;

pub(crate) fn build(cr: &MarsService, owner_ref: OwnerReference) -> Result<Option<PodDisruptionBudget>> {
    let (min_available, max_unavailable) = match cr.spec.runtime.pod_disruption_budget.as_ref() {
        // explicit override: honour the user's spec verbatim.
        Some(spec) => (
            spec.min_available.as_deref().map(parse_int_or_string),
            spec.max_unavailable.as_deref().map(parse_int_or_string),
        ),
        // no override + multi-replica: auto-default to maxUnavailable=1.
        None if cr.spec.runtime.replicas > 1 => (None, Some(IntOrString::Int(1))),
        // no override + single replica: a PDB would block every drain.
        None => return Ok(None),
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
            min_available,
            max_unavailable,
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

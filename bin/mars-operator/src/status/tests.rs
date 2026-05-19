#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn cond<'a>(s: &'a MarsServiceStatus, type_: &str) -> &'a Condition {
    s.conditions
        .iter()
        .find(|c| c.type_ == type_)
        .unwrap_or_else(|| panic!("missing condition {type_}: {:?}", s.conditions))
}

fn baseline_inputs<'a>(catalog: Resolution<'a>, definition: Resolution<'a>) -> StatusInputs<'a> {
    StatusInputs {
        observed_generation: 7,
        catalog,
        definition,
        definition_observed: None,
        config_valid: false,
        config_message: "blocked",
        children_applied: false,
        children_message: "skipped",
        compiler_ready: false,
        runtime_ready: false,
        degraded: None,
    }
}

#[test]
fn cluster_not_found_failure_skips_definition() {
    let inputs = baseline_inputs(
        Resolution::Failed {
            reason: ResolutionReason::ClusterNotFound,
            message: "cluster 'prod-eu' not found",
        },
        Resolution::Skipped {
            blocked_by: "CatalogResolved",
        },
    );
    let s = compute(inputs);
    let cat = cond(&s, "CatalogResolved");
    let def = cond(&s, "DefinitionResolved");
    assert_eq!(cat.status, "False");
    assert_eq!(cat.reason, "ClusterNotFound");
    assert!(cat.message.contains("prod-eu"));
    assert_eq!(def.status, "Unknown");
    assert_eq!(def.reason, "Skipped");
    assert!(def.message.contains("CatalogResolved"));
    assert!(s.definition.is_none());
}

#[test]
fn unknown_source_id_failure_keeps_definition_resolved() {
    let inputs = baseline_inputs(
        Resolution::Failed {
            reason: ResolutionReason::UnknownSourceId,
            message: "source 'foo' not in cluster catalog",
        },
        Resolution::Resolved,
    );
    let s = compute(inputs);
    let cat = cond(&s, "CatalogResolved");
    let def = cond(&s, "DefinitionResolved");
    assert_eq!(cat.status, "False");
    assert_eq!(cat.reason, "UnknownSourceId");
    assert_eq!(def.status, "True");
    assert_eq!(def.reason, "Resolved");
}

#[test]
fn definition_exactly_one_violated_emits_typed_reason() {
    let inputs = baseline_inputs(
        Resolution::Resolved,
        Resolution::Failed {
            reason: ResolutionReason::ExactlyOneViolated,
            message: "found 2 variants",
        },
    );
    let s = compute(inputs);
    let def = cond(&s, "DefinitionResolved");
    assert_eq!(def.status, "False");
    assert_eq!(def.reason, "ExactlyOneViolated");
}

#[test]
fn definition_fetch_error_emits_typed_reason() {
    let inputs = baseline_inputs(
        Resolution::Resolved,
        Resolution::Failed {
            reason: ResolutionReason::DefinitionFetchError,
            message: "s3 HEAD 403",
        },
    );
    let s = compute(inputs);
    let def = cond(&s, "DefinitionResolved");
    assert_eq!(def.status, "False");
    assert_eq!(def.reason, "DefinitionFetchError");
    assert!(def.message.contains("403"));
}

#[test]
fn happy_path_populates_definition_observed() {
    let mut inputs = baseline_inputs(Resolution::Resolved, Resolution::Resolved);
    inputs.definition_observed = Some(ObservedDefinition {
        adapter: "gitRef",
        revision: "abc1234",
    });
    inputs.config_valid = true;
    inputs.config_message = "ok";
    inputs.children_applied = true;
    inputs.children_message = "applied";
    inputs.compiler_ready = true;
    inputs.runtime_ready = true;
    let s = compute(inputs);
    assert_eq!(cond(&s, "CatalogResolved").status, "True");
    assert_eq!(cond(&s, "DefinitionResolved").status, "True");
    assert_eq!(cond(&s, "ConfigValid").status, "True");
    let observed = s.definition.expect("observed populated").observed;
    assert_eq!(observed.adapter, "gitRef");
    assert_eq!(observed.revision, "abc1234");
    assert_eq!(s.phase.as_deref(), Some("Ready"));
}

#[test]
fn observed_cleared_when_caller_omits_it() {
    let inputs = baseline_inputs(
        Resolution::Failed {
            reason: ResolutionReason::DefinitionDecodeError,
            message: "bad utf8",
        },
        Resolution::Failed {
            reason: ResolutionReason::DefinitionDecodeError,
            message: "bad utf8",
        },
    );
    let s = compute(inputs);
    assert!(s.definition.is_none());
}

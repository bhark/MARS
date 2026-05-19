#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::compose::ComposeError;
use crate::crd::spec::SpecValidationError;

fn assert_resolved(r: Resolution<'_>) {
    match r {
        Resolution::Resolved => {}
        _ => panic!("expected Resolved"),
    }
}

fn assert_failed_with(r: Resolution<'_>, want: &str) {
    match r {
        Resolution::Failed { reason, .. } => assert_eq!(reason.as_str(), want),
        _ => panic!("expected Failed({want})"),
    }
}

fn assert_skipped(r: Resolution<'_>) {
    match r {
        Resolution::Skipped { .. } => {}
        _ => panic!("expected Skipped"),
    }
}

#[test]
fn spec_validation_both_shapes_blocks_both() {
    let e = SpecValidationError::BothShapes;
    let msg = e.to_string();
    let (cat, def) = classify_spec_validation(&e, &msg);
    assert_failed_with(cat, "SpecInvalid");
    assert_failed_with(def, "SpecInvalid");
}

#[test]
fn spec_validation_missing_definition_blocks_only_definition() {
    let e = SpecValidationError::NewShapeMissing("definition");
    let msg = e.to_string();
    let (cat, def) = classify_spec_validation(&e, &msg);
    assert_resolved(cat);
    assert_failed_with(def, "SpecInvalid");
}

#[test]
fn spec_validation_missing_clusterref_blocks_catalog_and_skips_definition() {
    let e = SpecValidationError::NewShapeMissing("clusterRef");
    let msg = e.to_string();
    let (cat, def) = classify_spec_validation(&e, &msg);
    assert_failed_with(cat, "SpecInvalid");
    assert_skipped(def);
}

#[test]
fn spec_validation_exactly_one_maps_to_definition_failure() {
    let e = SpecValidationError::DefinitionVariantCount(2);
    let msg = e.to_string();
    let (cat, def) = classify_spec_validation(&e, &msg);
    assert_resolved(cat);
    assert_failed_with(def, "ExactlyOneViolated");
}

#[test]
fn resolve_error_cluster_not_found_blocks_catalog() {
    let e = OperatorError::ClusterNotFound("prod-eu".into());
    let msg = e.to_string();
    let (cat, def) = classify_resolve_error(&e, false, &msg);
    assert_failed_with(cat, "ClusterNotFound");
    assert_skipped(def);
}

#[test]
fn resolve_error_unknown_source_id_blocks_catalog_only() {
    let e = OperatorError::Compose(ComposeError::UnknownSourceId {
        id: "foo".into(),
        known: "bar".into(),
    });
    let msg = e.to_string();
    let (cat, def) = classify_resolve_error(&e, false, &msg);
    assert_failed_with(cat, "UnknownSourceId");
    assert_resolved(def);
}

#[test]
fn resolve_error_definition_decode_blocks_definition_only() {
    let e = OperatorError::DefinitionDecode("not utf-8".into());
    let msg = e.to_string();
    let (cat, def) = classify_resolve_error(&e, false, &msg);
    assert_resolved(cat);
    assert_failed_with(def, "DefinitionDecodeError");
}

#[test]
fn resolve_error_on_legacy_path_marks_catalog_legacy() {
    let e = OperatorError::MissingField("spec.config".into());
    let msg = e.to_string();
    let (cat, def) = classify_resolve_error(&e, true, &msg);
    assert!(matches!(cat, Resolution::Legacy));
    assert_failed_with(def, "Internal");
}

#[test]
fn observed_from_uses_resolved_definition_fields() {
    let rd = ResolvedDefinition {
        adapter: "gitRef",
        revision: "abc123".into(),
    };
    let o = observed_from(&rd);
    assert_eq!(o.adapter, "gitRef");
    assert_eq!(o.revision, "abc123");
}

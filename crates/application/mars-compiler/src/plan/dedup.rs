//! shape-consistency checks for shared bindings and layers.
//!
//! when two layers reference the same binding-id, or the same
//! `(layer_id, binding_id)` pair appears more than once, their plans must
//! agree field-for-field. divergence is rejected as a typed error so the
//! source/sidecar split stays sound (page artifacts must not need to know
//! which layer asked for them).

use super::error::PlanError;
use super::types::{BindingPlan, LayerPlan};

// short-circuit equality check; the error is built lazily so the hot path
// (fields agree) does no allocation.
fn ensure_eq<T: PartialEq>(a: &T, b: &T, err: impl FnOnce() -> PlanError) -> Result<(), PlanError> {
    if a == b { Ok(()) } else { Err(err()) }
}

pub(super) fn ensure_consistent(existing: &BindingPlan, candidate: &BindingPlan) -> Result<(), PlanError> {
    let conflict = |detail: &'static str| PlanError::ConflictingBinding {
        id: existing.binding_id.clone(),
        detail,
    };
    ensure_eq(&existing.source_id, &candidate.source_id, || conflict("source_id"))?;
    ensure_eq(&existing.geometry_field, &candidate.geometry_field, || {
        conflict("geometry_field")
    })?;
    ensure_eq(&existing.attributes, &candidate.attributes, || conflict("attributes"))?;
    ensure_eq(&existing.id_field, &candidate.id_field, || conflict("id_field"))?;
    ensure_eq(&existing.filter, &candidate.filter, || conflict("filter"))?;
    ensure_eq(&existing.levels, &candidate.levels, || conflict("levels"))?;
    ensure_eq(
        &existing.page_size_target_bytes,
        &candidate.page_size_target_bytes,
        || conflict("page_size_target_bytes"),
    )?;
    ensure_eq(
        &existing.sidecar_size_warn_bytes,
        &candidate.sidecar_size_warn_bytes,
        || conflict("sidecar_size_warn_bytes"),
    )?;
    ensure_eq(
        &existing.reconcile_every_cycles,
        &candidate.reconcile_every_cycles,
        || conflict("reconcile_every_cycles"),
    )?;
    ensure_eq(&existing.simplifier, &candidate.simplifier, || conflict("simplifier"))?;
    Ok(())
}

pub(super) fn ensure_layer_consistent(existing: &LayerPlan, candidate: &LayerPlan) -> Result<(), PlanError> {
    let conflict = |detail: &'static str| PlanError::ConflictingLayer {
        layer: existing.layer_id.clone(),
        binding: existing.binding_id.clone(),
        detail,
    };
    ensure_eq(&existing.kind, &candidate.kind, || conflict("kind"))?;
    ensure_eq(&existing.classes, &candidate.classes, || conflict("classes"))?;
    ensure_eq(&existing.label, &candidate.label, || conflict("label"))?;
    ensure_eq(&existing.label_survival, &candidate.label_survival, || {
        conflict("label_survival")
    })?;
    Ok(())
}

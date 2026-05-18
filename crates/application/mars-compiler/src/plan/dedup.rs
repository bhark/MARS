//! shape-consistency checks for shared bindings and layers.
//!
//! when two layers reference the same binding-id, or the same
//! `(layer_id, binding_id)` pair appears more than once, their plans must
//! agree field-for-field. divergence is rejected as a typed error so the
//! source/sidecar split stays sound (page artifacts must not need to know
//! which layer asked for them).

use super::error::PlanError;
use super::types::{BindingPlan, LayerPlan};

pub(super) fn ensure_consistent(existing: &BindingPlan, candidate: &BindingPlan) -> Result<(), PlanError> {
    if existing.source_id != candidate.source_id {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "source_id",
        });
    }
    if existing.geometry_field != candidate.geometry_field {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "geometry_field",
        });
    }
    if existing.attributes != candidate.attributes {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "attributes",
        });
    }
    if existing.id_field != candidate.id_field {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "id_field",
        });
    }
    if existing.filter != candidate.filter {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "filter",
        });
    }
    if existing.levels != candidate.levels {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "levels",
        });
    }
    if existing.page_size_target_bytes != candidate.page_size_target_bytes {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "page_size_target_bytes",
        });
    }
    if existing.sidecar_size_warn_bytes != candidate.sidecar_size_warn_bytes {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "sidecar_size_warn_bytes",
        });
    }
    if existing.reconcile_every_cycles != candidate.reconcile_every_cycles {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "reconcile_every_cycles",
        });
    }
    if existing.simplifier != candidate.simplifier {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "simplifier",
        });
    }
    Ok(())
}

pub(super) fn ensure_layer_consistent(existing: &LayerPlan, candidate: &LayerPlan) -> Result<(), PlanError> {
    if existing.kind != candidate.kind {
        return Err(PlanError::ConflictingLayer {
            layer: existing.layer_id.clone(),
            binding: existing.binding_id.clone(),
            detail: "kind",
        });
    }
    if existing.classes != candidate.classes {
        return Err(PlanError::ConflictingLayer {
            layer: existing.layer_id.clone(),
            binding: existing.binding_id.clone(),
            detail: "classes",
        });
    }
    if existing.label != candidate.label {
        return Err(PlanError::ConflictingLayer {
            layer: existing.layer_id.clone(),
            binding: existing.binding_id.clone(),
            detail: "label",
        });
    }
    if existing.label_survival != candidate.label_survival {
        return Err(PlanError::ConflictingLayer {
            layer: existing.layer_id.clone(),
            binding: existing.binding_id.clone(),
            detail: "label_survival",
        });
    }
    Ok(())
}

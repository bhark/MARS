//! [`BootstrapPlan`] orchestrator. walks the validated [`Config`], dedups
//! [`BindingPlan`]s by their resolved [`BindingId`], and emits one
//! [`LayerPlan`] per `(layer_id, binding_id)` pair. raster layers are
//! lifted to metadata-only [`RasterLayerEntry`] rows in one pass.

use mars_config::{Config, SourceId};
use mars_types::RasterLayerEntry;

use super::binding::build_binding_plan;
use super::dedup::{ensure_consistent, ensure_layer_consistent};
use super::error::PlanError;
use super::layer::build_layer_plan;
use super::types::{BindingPlan, BootstrapPlan, LayerPlan};

/// Build a [`BootstrapPlan`] from a validated config. dedup key is
/// `(from, geometry_field, attributes)`; a binding with no `levels:`
/// declared defaults to a single level-0 (raw) entry, since the snapshot
/// always materialises at least the canonical level.
pub fn build_bootstrap_plan(cfg: &Config) -> Result<BootstrapPlan, PlanError> {
    // index sources by id so per-binding native_crs lookup is O(log n).
    let sources_by_id: std::collections::BTreeMap<&SourceId, &mars_config::Source> =
        cfg.sources.iter().map(|s| (&s.id, s)).collect();
    let mut bindings: Vec<BindingPlan> = Vec::new();
    let mut layers: Vec<LayerPlan> = Vec::new();
    let raster_layers = build_raster_layer_entries(cfg);

    for layer in &cfg.layers {
        // raster layers are metadata-only at compile time; they have no
        // vector sources / classes / labels to enumerate. their manifest
        // entries come from build_raster_layer_entries above.
        if matches!(
            mars_style::LayerKind::parse(layer.kind.as_str()),
            Some(mars_style::LayerKind::Raster)
        ) {
            continue;
        }
        for binding in &layer.sources {
            let source = sources_by_id
                .get(&binding.source)
                .copied()
                .ok_or_else(|| PlanError::UnknownSourceRef {
                    from: binding.source_descriptor(),
                    source_id: binding.source.clone(),
                })?;
            let plan = build_binding_plan(source, binding)?;
            let id = plan.binding_id.clone();

            if let Some(existing) = bindings.iter().find(|b| b.binding_id == id) {
                ensure_consistent(existing, &plan)?;
            } else {
                bindings.push(plan);
            }

            let layer_plan = build_layer_plan(cfg, layer, &id)?;
            if let Some(existing) = layers
                .iter()
                .find(|l| l.layer_id == layer_plan.layer_id && l.binding_id == layer_plan.binding_id)
            {
                ensure_layer_consistent(existing, &layer_plan)?;
            } else {
                layers.push(layer_plan);
            }
        }
    }

    Ok(BootstrapPlan {
        bindings,
        layers,
        raster_layers,
    })
}

/// Translate every `kind: raster` layer in `cfg` into a [`RasterLayerEntry`]
/// for the manifest. Pure / total: validation has already enforced that
/// every raster-kind layer carries a well-formed `raster:` block, so this
/// function does not return an error type. Layers without a raster block
/// are skipped.
pub(crate) fn build_raster_layer_entries(cfg: &Config) -> Vec<RasterLayerEntry> {
    cfg.layers
        .iter()
        .filter_map(|layer| {
            let raster = layer.raster.as_ref()?;
            Some(RasterLayerEntry {
                layer_id: layer.name.clone(),
                collection: raster.source.collection.clone(),
                locator: raster.source.locator.clone(),
                source_crs: raster.source.source_crs.clone(),
                tile_size: raster.source.tile_size,
                max_level: raster.source.max_level,
                opacity: raster.opacity,
            })
        })
        .collect()
}

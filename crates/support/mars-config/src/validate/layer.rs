use std::collections::BTreeSet;

use crate::ConfigError;
use crate::model::{Config, Layer, RasterLayerSpec};
use crate::validate::band::{self, BandIndex};
use crate::validate::{attributes, binding, class, label};

pub(super) fn validate_layers(config: &Config, bands: &BandIndex) -> Result<(), ConfigError> {
    let mut layer_names: BTreeSet<&str> = BTreeSet::new();
    for layer in &config.layers {
        if !layer_names.insert(layer.name.as_str()) {
            return Err(ConfigError::Invalid(format!("duplicate layer name {:?}", layer.name)));
        }
        validate_layer(layer, config, bands)?;
    }
    Ok(())
}

fn validate_layer(layer: &Layer, config: &Config, bands: &BandIndex) -> Result<(), ConfigError> {
    validate_kind_raster_coherence(layer)?;
    if layer.raster.is_some() {
        // raster layers skip vector validation paths entirely - they share no
        // shape with the vector model (sources/classes/label are empty by
        // construction once kind/raster coherence has been verified).
        return Ok(());
    }
    class::validate_classes(layer, &config.styles)?;
    validate_sources(layer, bands)?;
    band::validate_band_tiers(layer, &bands.windows)?;
    attributes::validate_attribute_references(layer)?;
    label::validate_label(layer, &config.styles)?;
    Ok(())
}

/// raster coherence: kind == "raster" iff raster: is set. raster layers may
/// not carry vector sources/classes/label. raster spec must be well-formed
/// (opacity in [0,1], non-empty locator, positive tile size).
fn validate_kind_raster_coherence(layer: &Layer) -> Result<(), ConfigError> {
    let is_raster_kind = layer.kind.as_str() == "raster";
    match (is_raster_kind, layer.raster.as_ref()) {
        (true, None) => Err(ConfigError::Invalid(format!(
            "layer {} has type: raster but no raster: block",
            layer.name
        ))),
        (false, Some(_)) => Err(ConfigError::Invalid(format!(
            "layer {} has raster: block but type is {:?}, not raster",
            layer.name, layer.kind
        ))),
        (true, Some(spec)) => {
            if !layer.sources.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "raster layer {} must not declare vector sources",
                    layer.name
                )));
            }
            if !layer.classes.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "raster layer {} must not declare classes",
                    layer.name
                )));
            }
            if layer.label.is_some() {
                return Err(ConfigError::Invalid(format!(
                    "raster layer {} must not declare a label",
                    layer.name
                )));
            }
            validate_raster_spec(&layer.name, spec)
        }
        (false, None) => Ok(()),
    }
}

fn validate_raster_spec(layer_name: &mars_types::LayerId, spec: &RasterLayerSpec) -> Result<(), ConfigError> {
    if !spec.opacity.is_finite() || !(0.0..=1.0).contains(&spec.opacity) {
        return Err(ConfigError::Invalid(format!(
            "raster layer {layer_name} opacity {} out of range [0,1]",
            spec.opacity
        )));
    }
    if spec.source.locator.trim().is_empty() {
        return Err(ConfigError::Invalid(format!(
            "raster layer {layer_name} source.locator is empty"
        )));
    }
    if spec.source.collection.as_str().is_empty() {
        return Err(ConfigError::Invalid(format!(
            "raster layer {layer_name} source.collection is empty"
        )));
    }
    if spec.source.source_crs.as_str().is_empty() {
        return Err(ConfigError::Invalid(format!(
            "raster layer {layer_name} source.source_crs is empty"
        )));
    }
    if spec.source.tile_size == 0 {
        return Err(ConfigError::Invalid(format!(
            "raster layer {layer_name} source.tile_size must be > 0"
        )));
    }
    Ok(())
}

fn validate_sources(layer: &Layer, bands: &BandIndex) -> Result<(), ConfigError> {
    for (i, src) in layer.sources.iter().enumerate() {
        if let Some(band) = &src.band
            && !bands.names.contains(band.as_str())
        {
            return Err(ConfigError::Invalid(format!(
                "layer {} source[{i}] band {band:?} not declared in scales.bands",
                layer.name
            )));
        }
        if src.max_denom.is_some() && src.band.is_none() {
            return Err(ConfigError::Invalid(format!(
                "layer {} source[{i}] max_denom_exclusive requires a band",
                layer.name
            )));
        }
        binding::validate_binding_source(&layer.name, i, src)?;
        binding::validate_binding_levels(&layer.name, i, src)?;
    }
    Ok(())
}

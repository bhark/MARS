use std::collections::BTreeSet;

use crate::ConfigError;
use crate::model::{Config, Layer};
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
    class::validate_classes(layer, &config.styles)?;
    validate_sources(layer, bands)?;
    band::validate_band_tiers(layer, &bands.windows)?;
    attributes::validate_attribute_references(layer)?;
    label::validate_label(layer, &config.styles)?;
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
        binding::validate_binding_from(&layer.name, i, &src.from)?;
        binding::validate_binding_levels(&layer.name, i, src)?;
    }
    Ok(())
}

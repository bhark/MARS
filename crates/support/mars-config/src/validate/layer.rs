use std::collections::{BTreeMap, BTreeSet};

use crate::ConfigError;
use crate::model::{Layer, RasterLayerSpec, StyleEntry};
use crate::validate::band::{self, BandIndex};
use crate::validate::{attributes, binding, class, label};

pub(super) fn validate_layers(
    layers: &[Layer],
    styles: &BTreeMap<String, StyleEntry>,
    bands: &BandIndex,
) -> Result<(), ConfigError> {
    let mut layer_names: BTreeSet<&str> = BTreeSet::new();
    for layer in layers {
        if !layer_names.insert(layer.name.as_str()) {
            return Err(ConfigError::Invalid(format!("duplicate layer name {:?}", layer.name)));
        }
        validate_layer(layer, styles, bands)?;
    }
    Ok(())
}

fn validate_layer(layer: &Layer, styles: &BTreeMap<String, StyleEntry>, bands: &BandIndex) -> Result<(), ConfigError> {
    validate_kind_raster_coherence(layer)?;
    if layer.raster.is_some() {
        // raster layers skip vector validation paths entirely - they share no
        // shape with the vector model (sources/classes/label are empty by
        // construction once kind/raster coherence has been verified).
        return Ok(());
    }
    class::validate_classes(layer, styles)?;
    validate_sources(layer, bands)?;
    band::validate_band_tiers(layer, &bands.windows)?;
    attributes::validate_attribute_references(layer)?;
    label::validate_label(layer, styles)?;
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

/// Tile-edge pixel counts the render path accepts. Web Mercator slippy-map
/// pyramids are universally 256 or 512; anything else is almost certainly a
/// configuration mistake worth catching before runtime.
const SUPPORTED_RASTER_TILE_SIZES: &[u32] = &[256, 512];

/// Upper bound on `max_level`. 2^24 tiles per side covers any practical
/// pyramid with vast headroom while staying well below the u32 / u64 ranges
/// the tile-math uses. Caught here so `mars validate` rejects nonsense like
/// 99 before runtime hits an integer-overflow path.
const RASTER_MAX_LEVEL_CAP: u32 = 24;

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
    // locator placeholder shape (e.g. {z}/{x}/{y} for XYZ) is adapter-specific;
    // we leave that check to the adapter so the validator stays adapter-agnostic.
    let crs = spec.source.source_crs.as_str();
    if crs.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "raster layer {layer_name} source.source_crs is empty"
        )));
    }
    if !spec.source.source_crs.is_supported_raster() {
        let supported = mars_types::CrsCode::SUPPORTED_RASTER;
        return Err(ConfigError::Invalid(format!(
            "raster layer {layer_name} source.source_crs {crs:?} is not supported (expected one of {supported:?})"
        )));
    }
    if !SUPPORTED_RASTER_TILE_SIZES.contains(&spec.source.tile_size) {
        return Err(ConfigError::Invalid(format!(
            "raster layer {layer_name} source.tile_size {} is not supported (expected one of {SUPPORTED_RASTER_TILE_SIZES:?})",
            spec.source.tile_size
        )));
    }
    if spec.source.max_level > RASTER_MAX_LEVEL_CAP {
        return Err(ConfigError::Invalid(format!(
            "raster layer {layer_name} source.max_level {} exceeds the cap of {RASTER_MAX_LEVEL_CAP}",
            spec.source.max_level
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
        binding::validate_binding_levels(&layer.name, i, src)?;
    }
    Ok(())
}

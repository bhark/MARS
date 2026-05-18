use std::path::Path;

use crate::ConfigError;
use crate::model::Config;

mod attributes;
mod band;
mod binding;
mod class;
mod compiler;
mod crs;
mod label;
mod layer;
mod service;
mod source;
mod style;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub(crate) mod fixtures;

/// Validate a parsed configuration and resolve derived forms in place.
///
/// Cross-cutting checks beyond serde:
/// - every layer's `style: { ref: ... }` resolves against `styles`;
/// - every source binding's `band` (when set) exists in `scales.bands`;
/// - every `cells.size_per_band` key matches a declared band;
/// - every class `when:` parses via [`mars_expr::parse`].
///
/// Resolution step: every source binding with `band: Some(name)` has its
/// `scale: ScaleWindow` intersected with the band's half-open denominator
/// interval (Glossary - bands are routing rules). Disjoint intersections are
/// rejected so the renderer's binding picker, which consumes `source.scale`
/// directly, sees the effective routing window without needing band knowledge.
///
/// `config_dir` is currently unused at validate time but accepted for symmetry
/// and future-proofing - validation may grow filesystem checks (e.g. cache
/// path writability) that require it.
pub fn validate(config: &mut Config, config_dir: &Path) -> Result<(), ConfigError> {
    let _ = config_dir;

    service::validate_service(config)?;
    compiler::validate_compiler_and_render(config)?;
    source::validate_sources(config)?;
    crs::validate_native_crs(config)?;
    style::validate_styles(&config.styles)?;

    let bands = band::validate_bands(config)?;
    layer::validate_layers(config, &bands)?;

    band::resolve_band_routing(config)?;
    Ok(())
}

#[cfg(test)]
mod tests;

use std::path::Path;

use crate::ConfigError;
use crate::model::{Config, Deployment, RenderDefinition};

mod attributes;
mod band;
mod binding;
mod class;
mod compiler;
mod crs;
mod label;
mod layer;
mod reprojection;
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
/// - every class `when:` parses via [`mars_expr::parse`];
/// - every tile-matrix-set CRS is reachable from at least one source or via
///   the `reprojection.allowlist`.
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

    // definition-side
    service::validate_service(&config.service)?;
    style::validate_styles(&config.styles)?;
    let bands = band::validate_bands(&config.scales)?;
    layer::validate_layers(&config.layers, &config.styles, &bands)?;

    // deployment-side
    source::validate_sources(&config.sources)?;
    crs::validate_native_crs(&config.sources)?;
    compiler::validate_compiler_and_render(&config.compiler, &config.render)?;

    // cross-cutting: needs both halves
    binding::validate_binding_source_refs(&config.layers, &config.sources)?;
    reprojection::validate_reprojection_coherence(&config.reprojection, &config.tile_matrix_sets, &config.sources)?;

    // resolution (mutates layers)
    band::resolve_band_routing(&config.scales, &mut config.layers)?;
    Ok(())
}

/// Definition-side validation: render-only fields. Excludes any check that
/// depends on a `Source` catalog or other deployment-side data. Mutates the
/// definition only to resolve band routing windows on the layers.
pub(crate) fn validate_render_definition(def: &mut RenderDefinition) -> Result<(), ConfigError> {
    service::validate_service(&def.service)?;
    style::validate_styles(&def.styles)?;
    let bands = band::validate_bands(&def.scales)?;
    layer::validate_layers(&def.layers, &def.styles, &bands)?;
    band::resolve_band_routing(&def.scales, &mut def.layers)?;
    Ok(())
}

/// Deployment-side validation: env-shaped fields. Excludes any check that
/// depends on the render definition (layers, styles, scales).
pub(crate) fn validate_deployment(dep: &Deployment) -> Result<(), ConfigError> {
    source::validate_sources(&dep.sources)?;
    crs::validate_native_crs(&dep.sources)?;
    compiler::validate_compiler_and_render(&dep.compiler, &dep.render)?;
    Ok(())
}

#[cfg(test)]
mod tests;

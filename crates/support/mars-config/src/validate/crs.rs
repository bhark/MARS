use crate::ConfigError;
use crate::model::Source;

/// Validate that `code` is a projected (metric) CRS using PROJ introspection.
/// PROJ failures (broken install, missing proj.db) surface as
/// `ConfigError::ProjUnavailable` rather than collapsing into "not metric",
/// which would mislead operators into thinking they configured the wrong CRS.
pub(super) fn is_metric_crs(code: &str) -> Result<bool, ConfigError> {
    let trimmed = code.trim();
    let crs = mars_types::CrsCode::new(trimmed);
    mars_proj::is_projected(&crs).map_err(|source| ConfigError::ProjUnavailable {
        code: trimmed.to_string(),
        source,
    })
}

/// Each source's `native_crs` must be a recognised metric CRS, since the
/// runtime materialises artifacts in that CRS and reprojects to request CRSes
/// from there. Geographic CRSes (degrees) break the units-per-metre = 1
/// invariant the renderer relies on.
pub(super) fn validate_native_crs(sources: &[Source]) -> Result<(), ConfigError> {
    for src in sources {
        let crs = src.native_crs.as_str().trim();
        if crs.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "sources[{:?}].native_crs must not be empty",
                src.id.as_str()
            )));
        }
        if !is_metric_crs(crs)? {
            return Err(ConfigError::Invalid(format!(
                "sources[{:?}].native_crs {crs:?} is not a recognised metric CRS; mars-runtime requires a metric \
                 canonical CRS (units-per-metre = 1). Use a projected, metre-based EPSG code (e.g. EPSG:25832, \
                 EPSG:3857).",
                src.id.as_str()
            )));
        }
    }
    Ok(())
}

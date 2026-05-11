use crate::ConfigError;
use crate::model::Config;

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

pub(super) fn validate_native_crs(config: &Config) -> Result<(), ConfigError> {
    let crs = config.source.native_crs.as_str().trim();
    if crs.is_empty() {
        return Err(ConfigError::Invalid("source.native_crs must not be empty".into()));
    }
    if !is_metric_crs(crs)? {
        return Err(ConfigError::Invalid(format!(
            "source.native_crs {crs:?} is not a recognised metric CRS; mars-runtime requires a metric canonical CRS \
             (units-per-metre = 1). Use a projected, metre-based EPSG code (e.g. EPSG:25832, EPSG:3857)."
        )));
    }
    Ok(())
}

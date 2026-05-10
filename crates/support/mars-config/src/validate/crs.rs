use crate::ConfigError;

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

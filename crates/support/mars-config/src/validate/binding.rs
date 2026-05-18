use mars_types::LayerId;

use crate::ConfigError;
use crate::model::{Config, SourceBackend, SourceBinding};

/// Resolve the `source:` field on `binding` against the service-level
/// sources list. Returns the resolved kind so the binding-shape check can
/// cross-verify (postgis source ↔ from/sql binding, vectorfile source ↔ uri
/// binding).
fn resolve_binding_source<'cfg>(
    layer: &LayerId,
    idx: usize,
    binding: &SourceBinding,
    config: &'cfg Config,
) -> Result<&'cfg SourceBackend, ConfigError> {
    let source_id = &binding.source;
    config
        .sources
        .iter()
        .find(|s| &s.id == source_id)
        .map(|s| &s.backend)
        .ok_or_else(|| {
            ConfigError::Invalid(format!(
                "layer {layer} source[{idx}] references unknown source {:?}; declare it under top-level `sources:`",
                source_id.as_str()
            ))
        })
}

/// Validate the source-shape coherence of a binding: exactly one of `from:`
/// (table reference), `sql:` (inline SELECT) or `uri:` (vector file) must be
/// set, and the variant must match the kind of the referenced source.
pub(super) fn validate_binding_source(
    layer: &LayerId,
    idx: usize,
    binding: &SourceBinding,
    config: &Config,
) -> Result<(), ConfigError> {
    let backend = resolve_binding_source(layer, idx, binding, config)?;
    let from = binding.from.as_deref();
    let sql = binding.sql.as_deref();
    let uri = binding.uri.as_deref();

    // per-binding DSN override is postgis-only; reject early on vectorfile so
    // the rest of the validation can ignore the field.
    if binding.dsn.is_some() && !matches!(backend, SourceBackend::Postgis(_)) {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] sets `dsn:` override but references a non-postgis source {:?}",
            binding.source.as_str()
        )));
    }

    let variants_set = [from.is_some(), sql.is_some(), uri.is_some()]
        .iter()
        .filter(|b| **b)
        .count();
    if variants_set == 0 {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] must set exactly one of `from:`, `sql:` or `uri:`"
        )));
    }
    if variants_set > 1 {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] sets more than one of `from:`, `sql:`, `uri:`; they are mutually exclusive"
        )));
    }

    match (backend, from, sql, uri) {
        (SourceBackend::Postgis(_), Some(t), None, None) => validate_table_from(layer, idx, t),
        (SourceBackend::Postgis(_), None, Some(s), None) => validate_sql_binding(layer, idx, s),
        (SourceBackend::Postgis(_), None, None, Some(_)) => Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] sets `uri:` but references a postgis source {:?}; use `from:` or `sql:`",
            binding.source.as_str()
        ))),
        (SourceBackend::VectorFile(_), None, None, Some(_)) => validate_vectorfile_binding(layer, idx, binding),
        (SourceBackend::VectorFile(_), _, _, _) => Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] references vectorfile source {:?} but uses a postgis binding shape; use `uri:` \
             + `format:` + `source_crs:`",
            binding.source.as_str()
        ))),
        _ => unreachable!("variants_set check above"),
    }
}

/// Validate that every per-layer binding's `source:` resolves and the source
/// kind matches the binding shape. Wraps [`validate_binding_source`] across
/// the layer set.
pub(super) fn validate_binding_source_refs(config: &Config) -> Result<(), ConfigError> {
    for layer in &config.layers {
        for (idx, binding) in layer.sources.iter().enumerate() {
            validate_binding_source(&layer.name, idx, binding, config)?;
        }
    }
    Ok(())
}

#[cfg_attr(test, allow(dead_code))]
fn validate_vectorfile_binding(layer: &LayerId, idx: usize, binding: &SourceBinding) -> Result<(), ConfigError> {
    let uri = binding.uri.as_deref().unwrap_or_default().trim();
    if uri.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] uri must not be empty"
        )));
    }
    if !uri_scheme_supported(uri) {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] uri {uri:?} must start with one of s3://, gs://, file://, http://, https://"
        )));
    }
    if binding.format.is_none() {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] vectorfile binding requires `format:`"
        )));
    }
    let Some(src_crs) = binding.source_crs.as_ref() else {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] vectorfile binding requires `source_crs:`"
        )));
    };
    if src_crs.as_str().trim().is_empty() {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] source_crs must not be empty"
        )));
    }
    Ok(())
}

fn uri_scheme_supported(uri: &str) -> bool {
    ["s3://", "gs://", "file://", "http://", "https://"]
        .iter()
        .any(|s| uri.starts_with(s))
}

fn validate_table_from(layer: &LayerId, idx: usize, from: &str) -> Result<(), ConfigError> {
    let trimmed = from.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] from must not be empty"
        )));
    }
    let lower = trimmed.to_ascii_lowercase();
    let bad_substr = ["(", ")", ";", " join ", " select ", " from ", " where ", " union "]
        .iter()
        .find(|s| lower.contains(*s));
    if let Some(needle) = bad_substr {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] from {from:?} is not a single-table reference \
             (contains {needle:?}); use `sql:` for inline SELECTs or name one table"
        )));
    }
    // single-segment names route to `public`; allow at most `schema.table`.
    if trimmed.matches('.').count() > 1 {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] from {from:?} must be `table` or `schema.table`"
        )));
    }
    Ok(())
}

fn validate_sql_binding(layer: &LayerId, idx: usize, sql: &str) -> Result<(), ConfigError> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] sql must not be empty"
        )));
    }
    let lower = trimmed.to_ascii_lowercase();
    if !lower.starts_with("select") && !lower.starts_with("(select") {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] sql must be a SELECT statement; got {sql:?}"
        )));
    }
    // semicolon-terminated statements would expose the snapshot subquery to
    // statement injection; reject anything that closes the SELECT.
    if trimmed.contains(';') {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] sql contains `;`; only a single SELECT is permitted"
        )));
    }
    Ok(())
}

/// Validate per-level decimation config on a single binding.
pub(super) fn validate_binding_levels(layer: &LayerId, idx: usize, binding: &SourceBinding) -> Result<(), ConfigError> {
    if let Some(target) = binding.page_size_target_bytes
        && target == 0
    {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] page_size_target_bytes must be > 0"
        )));
    }
    if let Some(cycles) = binding.reconcile_every_cycles
        && cycles == 0
    {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] reconcile_every_cycles must be > 0"
        )));
    }
    let warn_bytes = binding
        .resolved_sidecar_size_warn_bytes()
        .map_err(|e| ConfigError::Invalid(format!("layer {layer} source[{idx}] sidecar_size_warn_bytes: {e}")))?;
    if warn_bytes == 0 {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] sidecar_size_warn_bytes must be > 0"
        )));
    }
    let Some(levels) = &binding.levels else {
        return Ok(());
    };
    if levels.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "layer {layer} source[{idx}] levels: must not be empty when set"
        )));
    }
    let mut prev_level: Option<u8> = None;
    let mut prev_vertex_tol: Option<f64> = None;
    let mut prev_min_size: Option<f64> = None;
    let mut prev_label_prio: Option<u32> = None;
    for (li, lvl) in levels.iter().enumerate() {
        if let Some(p) = prev_level
            && lvl.level <= p
        {
            return Err(ConfigError::Invalid(format!(
                "layer {layer} source[{idx}] levels[{li}] level {} must be strictly greater than previous {}",
                lvl.level, p
            )));
        }
        prev_level = Some(lvl.level);
        if !lvl.vertex_tolerance_m.is_finite() || lvl.vertex_tolerance_m < 0.0 {
            return Err(ConfigError::Invalid(format!(
                "layer {layer} source[{idx}] levels[{li}] vertex_tolerance_m must be finite and >= 0"
            )));
        }
        if !lvl.geometry_min_size_m.is_finite() || lvl.geometry_min_size_m < 0.0 {
            return Err(ConfigError::Invalid(format!(
                "layer {layer} source[{idx}] levels[{li}] geometry_min_size_m must be finite and >= 0"
            )));
        }
        // monotone non-decreasing across the level sequence: higher levels
        // are coarser. lets the page-rebuild path reason about decimation
        // sets as a strict refinement chain (level n's surviving features
        // are a superset of level n+1's).
        if let Some(p) = prev_vertex_tol
            && lvl.vertex_tolerance_m < p
        {
            return Err(ConfigError::Invalid(format!(
                "layer {layer} source[{idx}] levels[{li}] vertex_tolerance_m {} must be >= previous {}",
                lvl.vertex_tolerance_m, p
            )));
        }
        if let Some(p) = prev_min_size
            && lvl.geometry_min_size_m < p
        {
            return Err(ConfigError::Invalid(format!(
                "layer {layer} source[{idx}] levels[{li}] geometry_min_size_m {} must be >= previous {}",
                lvl.geometry_min_size_m, p
            )));
        }
        if let Some(p) = prev_label_prio
            && lvl.label_min_priority < p
        {
            return Err(ConfigError::Invalid(format!(
                "layer {layer} source[{idx}] levels[{li}] label_min_priority {} must be >= previous {}",
                lvl.label_min_priority, p
            )));
        }
        prev_vertex_tol = Some(lvl.vertex_tolerance_m);
        prev_min_size = Some(lvl.geometry_min_size_m);
        prev_label_prio = Some(lvl.label_min_priority);
    }
    Ok(())
}

#[cfg(test)]
mod tests;

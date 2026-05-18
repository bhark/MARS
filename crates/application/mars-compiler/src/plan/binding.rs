//! per-binding plan helpers. covers binding-id derivation, source-locator
//! shaping (postgis from:/sql: vs vectorfile uri:), and the default-level
//! collapse used when a binding declares no `levels:` block.

use mars_config::DecimationLevelConfig;
use mars_types::{BindingId, DecimationLevel};

use super::error::PlanError;
use super::types::LevelPlan;

/// Stable level plan list. an absent `levels:` config collapses to a single
/// level-0 entry with zero decimation -- preserves the canonical raw set.
pub(super) fn level_plans(cfg_levels: Option<&[DecimationLevelConfig]>) -> Vec<LevelPlan> {
    match cfg_levels {
        Some(list) if !list.is_empty() => list
            .iter()
            .map(|l| LevelPlan {
                level: DecimationLevel::new(l.level),
                vertex_tolerance_m: l.vertex_tolerance_m,
                geometry_min_size_m: l.geometry_min_size_m,
                label_min_priority: l.label_min_priority,
            })
            .collect(),
        _ => vec![LevelPlan {
            level: DecimationLevel::new(0),
            vertex_tolerance_m: 0.0,
            geometry_min_size_m: 0.0,
            label_min_priority: 0,
        }],
    }
}

pub(super) fn binding_id_for(from: &str) -> Result<BindingId, PlanError> {
    BindingId::try_new(from).map_err(|source| PlanError::InvalidBindingId {
        from: from.to_owned(),
        source,
    })
}

/// Resolve a config binding to its (locator, id) pair. Postgis table form
/// passes the `from:` string through unchanged; sql form wraps the inline
/// SELECT in parens (so the postgres adapter can splice it into `FROM (...)
/// AS s`) and derives a stable, hash-prefixed `BindingId` so equal SELECTs
/// across layers dedupe. Vectorfile form (`uri:` + `format:` + `source_crs:`)
/// embeds the format / source_crs as a `#format=...&source_crs=...` fragment
/// on the URI so the adapter sees one opaque locator and ids dedupe per
/// (uri, format, source_crs) triple.
pub(super) fn resolve_binding_source(binding: &mars_config::SourceBinding) -> Result<(String, BindingId), PlanError> {
    if let Some(from) = binding.from.as_deref() {
        let id = binding_id_for(from)?;
        return Ok((from.to_owned(), id));
    }
    if let Some(sql) = binding.sql.as_deref() {
        let hash = blake3::hash(sql.as_bytes()).to_hex();
        let id_str = format!("sql_{}", &hash.as_str()[..16]);
        let id = binding_id_for(&id_str)?;
        return Ok((format!("({sql})"), id));
    }
    if let Some(uri) = binding.uri.as_deref() {
        let fmt = binding.format.ok_or_else(|| PlanError::IncompleteVectorFileBinding {
            from: binding.source_descriptor(),
            what: "format",
        })?;
        let source_crs = binding
            .source_crs
            .as_ref()
            .ok_or_else(|| PlanError::IncompleteVectorFileBinding {
                from: binding.source_descriptor(),
                what: "source_crs",
            })?;
        let fmt_tok = match fmt {
            mars_config::VectorFileFormat::FlatGeobuf => "flat_geobuf",
            mars_config::VectorFileFormat::GeoJson => "geo_json",
            mars_config::VectorFileFormat::Shapefile => "shapefile",
            mars_config::VectorFileFormat::GeoPackage => "geo_package",
        };
        let locator = format!("{uri}#format={fmt_tok}&source_crs={}", source_crs.as_str());
        // BindingId must be path-safe; hash the locator so URIs with colons /
        // slashes still produce a valid id. dedup key matches (uri, format, source_crs).
        let hash = blake3::hash(locator.as_bytes()).to_hex();
        let id_str = format!("vf_{}", &hash.as_str()[..16]);
        let id = binding_id_for(&id_str)?;
        return Ok((locator, id));
    }
    // config validation rejects bindings with neither from: nor sql: nor
    // uri:; surface a typed error in case a config bypasses validate.
    Err(PlanError::BindingSourceUnspecified {
        descriptor: binding.source_descriptor(),
    })
}

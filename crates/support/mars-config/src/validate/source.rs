use std::collections::HashSet;

use crate::ConfigError;
use crate::model::{Bootstrap, ChangeFeed, PostgisBackend, Source, SourceBackend, VectorFileBackend};

// keep identifiers safe to interpolate without escaping surprises. mirrors what
// quote_ident in mars-source-postgres accepts; the validator rejects early.
fn is_valid_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Service-level source validation: non-empty list, unique ids,
/// per-backend coherence (postgis bootstrap requires change_feed,
/// vectorfile cache_dir non-empty, etc.).
pub(super) fn validate_sources(sources: &[Source]) -> Result<(), ConfigError> {
    if sources.is_empty() {
        return Err(ConfigError::Invalid("sources: must declare at least one source".into()));
    }
    let mut seen: HashSet<String> = HashSet::new();
    for src in sources {
        let id = src.id.as_str();
        if id.trim().is_empty() {
            return Err(ConfigError::Invalid("sources[].id must not be empty".into()));
        }
        if !seen.insert(id.to_string()) {
            return Err(ConfigError::Invalid(format!(
                "sources[].id {id:?} declared more than once"
            )));
        }
        validate_native_crs_field(src)?;
        match &src.backend {
            SourceBackend::Postgis(pg) => validate_postgis(id, pg)?,
            SourceBackend::VectorFile(vf) => validate_vectorfile(id, vf)?,
        }
    }
    Ok(())
}

fn validate_native_crs_field(src: &Source) -> Result<(), ConfigError> {
    let crs = src.native_crs.as_str().trim();
    if crs.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "sources[{:?}].native_crs must not be empty",
            src.id.as_str()
        )));
    }
    Ok(())
}

fn validate_postgis(id: &str, pg: &PostgisBackend) -> Result<(), ConfigError> {
    if pg.dsn.trim().is_empty() {
        return Err(ConfigError::Invalid(format!(
            "sources[{id:?}].dsn must not be empty for postgis source"
        )));
    }
    let Some(bs) = pg.bootstrap.as_ref() else {
        return Ok(());
    };
    let Some(cf) = pg.change_feed.as_ref() else {
        return Err(ConfigError::Invalid(format!(
            "sources[{id:?}].bootstrap requires change_feed to be configured"
        )));
    };
    validate_bootstrap_block(id, bs, cf)
}

fn validate_bootstrap_block(id: &str, bs: &Bootstrap, cf: &ChangeFeed) -> Result<(), ConfigError> {
    if cf.kind != "pgoutput" {
        return Err(ConfigError::Invalid(format!(
            "sources[{id:?}].bootstrap requires change_feed.type = \"pgoutput\"; got {:?}",
            cf.kind
        )));
    }
    if cf.publication.as_deref().unwrap_or("").is_empty() {
        return Err(ConfigError::Invalid(format!(
            "sources[{id:?}].bootstrap requires change_feed.publication"
        )));
    }
    if cf.slot.as_deref().unwrap_or("").is_empty() {
        return Err(ConfigError::Invalid(format!(
            "sources[{id:?}].bootstrap requires change_feed.slot"
        )));
    }
    if !is_valid_ident(&bs.role) {
        return Err(ConfigError::Invalid(format!(
            "sources[{id:?}].bootstrap.role {:?} must match [a-z_][a-z0-9_]*",
            bs.role
        )));
    }
    if let Some(pub_name) = cf.publication.as_deref()
        && !is_valid_ident(pub_name)
    {
        return Err(ConfigError::Invalid(format!(
            "sources[{id:?}].change_feed.publication {pub_name:?} must match [a-z_][a-z0-9_]*"
        )));
    }
    if let Some(slot) = cf.slot.as_deref()
        && !is_valid_ident(slot)
    {
        return Err(ConfigError::Invalid(format!(
            "sources[{id:?}].change_feed.slot {slot:?} must match [a-z_][a-z0-9_]*"
        )));
    }
    if bs.schemas.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "sources[{id:?}].bootstrap.schemas must be non-empty"
        )));
    }
    for s in &bs.schemas {
        if !is_valid_ident(s) {
            return Err(ConfigError::Invalid(format!(
                "sources[{id:?}].bootstrap.schemas entry {s:?} must match [a-z_][a-z0-9_]*"
            )));
        }
    }
    Ok(())
}

fn validate_vectorfile(id: &str, vf: &VectorFileBackend) -> Result<(), ConfigError> {
    if vf.cache_dir.trim().is_empty() {
        return Err(ConfigError::Invalid(format!(
            "sources[{id:?}].cache_dir must not be empty for vectorfile source"
        )));
    }
    // both parse-side checks: any unit-parse error surfaces as the propagated
    // ConfigError from parse_duration / parse_bytes.
    let _ = vf.poll_interval_dur()?;
    let _ = vf.cache_max_size_bytes()?;
    Ok(())
}

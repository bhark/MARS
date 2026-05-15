use crate::ConfigError;
use crate::model::Config;

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

pub(super) fn validate_bootstrap(config: &Config) -> Result<(), ConfigError> {
    let Some(bs) = config.source.bootstrap.as_ref() else {
        return Ok(());
    };
    let Some(cf) = config.source.change_feed.as_ref() else {
        return Err(ConfigError::Invalid(
            "source.bootstrap requires source.change_feed to be configured".into(),
        ));
    };
    if cf.kind != "pgoutput" {
        return Err(ConfigError::Invalid(format!(
            "source.bootstrap requires change_feed.type = \"pgoutput\"; got {:?}",
            cf.kind
        )));
    }
    if cf.publication.as_deref().unwrap_or("").is_empty() {
        return Err(ConfigError::Invalid(
            "source.bootstrap requires change_feed.publication".into(),
        ));
    }
    if cf.slot.as_deref().unwrap_or("").is_empty() {
        return Err(ConfigError::Invalid(
            "source.bootstrap requires change_feed.slot".into(),
        ));
    }
    if !is_valid_ident(&bs.role) {
        return Err(ConfigError::Invalid(format!(
            "source.bootstrap.role {:?} must match [a-z_][a-z0-9_]*",
            bs.role
        )));
    }
    if let Some(pub_name) = cf.publication.as_deref()
        && !is_valid_ident(pub_name)
    {
        return Err(ConfigError::Invalid(format!(
            "source.change_feed.publication {pub_name:?} must match [a-z_][a-z0-9_]*"
        )));
    }
    if let Some(slot) = cf.slot.as_deref()
        && !is_valid_ident(slot)
    {
        return Err(ConfigError::Invalid(format!(
            "source.change_feed.slot {slot:?} must match [a-z_][a-z0-9_]*"
        )));
    }
    if bs.schemas.is_empty() {
        return Err(ConfigError::Invalid(
            "source.bootstrap.schemas must be non-empty".into(),
        ));
    }
    for s in &bs.schemas {
        if !is_valid_ident(s) {
            return Err(ConfigError::Invalid(format!(
                "source.bootstrap.schemas entry {s:?} must match [a-z_][a-z0-9_]*"
            )));
        }
    }
    Ok(())
}

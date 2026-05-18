//! Validate and canonicalise the `spec.config` blob.
//!
//! The operator writes the YAML to a ConfigMap verbatim (placeholders
//! preserved) so the pod's `mars_config::load` performs env substitution at
//! startup. Before writing we still need to be confident the document parses
//! into `mars_config::Config` once placeholders have been replaced - otherwise
//! a typo lands a Deployment that crashloops with no operator-side signal.
//!
//! Strategy:
//! 1. Serialise the JSON value to canonical YAML (stable key ordering).
//! 2. Substitute every `${VAR}` / `${VAR:-default}` token with a sentinel
//!    string so the downstream parse never trips on an unset env var. This
//!    is structural validation only: actual values are substituted by the
//!    pod at runtime against the real environment.
//! 3. Parse the sentinel-substituted YAML into `mars_config::Config`.

use serde_json::Value as JsonValue;

use crate::error::{OperatorError, Result};

/// Render the JSON value the user supplied as canonical YAML (sorted keys).
/// `serde_yaml_ng` sorts maps deterministically when fed a BTreeMap-backed
/// document; we route through `serde_json::Value -> serde_yaml_ng::Value`
/// to inherit that ordering.
pub(crate) fn canonicalize_yaml(value: &JsonValue) -> Result<String> {
    let yaml_value: serde_yaml_ng::Value = serde_yaml_ng::to_value(value)?;
    let out = serde_yaml_ng::to_string(&yaml_value)?;
    Ok(out)
}

/// Validate that `spec.config` parses into `mars_config::Config`. Placeholders
/// are substituted with sentinels so unset env vars do not fail validation.
pub(crate) fn validate(value: &JsonValue) -> Result<()> {
    let yaml = canonicalize_yaml(value)?;
    let sanitised = strip_placeholders(&yaml);

    let mut parsed: mars_config::Config = serde_yaml_ng::from_str(&sanitised).map_err(|e| {
        OperatorError::ConfigInvalid(format!(
            "spec.config does not deserialise into mars_config::Config: {e}"
        ))
    })?;

    // validate() requires a config dir for symmetry with the on-disk loader.
    // we have no real path here; pass "/" which is harmless because validate
    // does not currently consult the filesystem.
    mars_config::validate(&mut parsed, std::path::Path::new("/"))
        .map_err(|e| OperatorError::ConfigInvalid(e.to_string()))?;
    Ok(())
}

/// Replace `${VAR}` / `${VAR:-default}` tokens with a benign sentinel so the
/// post-parse Config does not require the operator to know which env vars
/// the pod will see. The sentinel is structural - just enough to keep the
/// YAML parse + serde deserialise honest.
///
/// Scans for `$`, `{`, and `}` directly on the `&str`: all three are
/// single-byte ASCII, so splitting at their byte offsets is safe even when
/// surrounding content includes multi-byte UTF-8 (label text, comments, ...).
fn strip_placeholders(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut remaining = src;
    while let Some(idx) = remaining.find('$') {
        out.push_str(&remaining[..idx]);
        let after = &remaining[idx + 1..];
        if let Some(rest) = after.strip_prefix('$') {
            // $$ -> literal $
            out.push('$');
            remaining = rest;
            continue;
        }
        if let Some(rest) = after.strip_prefix('{') {
            if let Some(close) = rest.find('}') {
                out.push_str("MARS_OPERATOR_PLACEHOLDER");
                remaining = &rest[close + 1..];
                continue;
            }
            // unclosed placeholder: emit the bare '$' and continue scanning
            // from the '{'. mirrors the prior byte-loop fallthrough.
            out.push('$');
            remaining = after;
            continue;
        }
        out.push('$');
        remaining = after;
    }
    out.push_str(remaining);
    out
}

#[cfg(test)]
mod tests;

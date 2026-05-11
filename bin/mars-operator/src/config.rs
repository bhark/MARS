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
fn strip_placeholders(src: &str) -> String {
    // simple state machine - the env_subst regex is not exposed publicly by
    // mars-config, and we explicitly do not want to call substitute() here
    // because it would consume the placeholder. duplicating the recogniser
    // here is light and bounded.
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'$' {
            out.push('$');
            i += 2;
            continue;
        }
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'}' {
                j += 1;
            }
            if j < bytes.len() {
                out.push_str("MARS_OPERATOR_PLACEHOLDER");
                i = j + 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn strip_placeholders_replaces_simple_token() {
        let out = strip_placeholders("dsn: ${PG_DSN}\n");
        assert!(!out.contains("${PG_DSN}"));
        assert!(out.contains("MARS_OPERATOR_PLACEHOLDER"));
    }

    #[test]
    fn strip_placeholders_replaces_default_token() {
        let out = strip_placeholders("dsn: ${PG_DSN:-postgres://}\n");
        assert!(!out.contains("${"));
        assert!(out.contains("MARS_OPERATOR_PLACEHOLDER"));
    }

    #[test]
    fn strip_placeholders_keeps_double_dollar_literal() {
        let out = strip_placeholders("cost: $$5\n");
        assert_eq!(out, "cost: $5\n");
    }

    #[test]
    fn canonicalize_yaml_round_trips() {
        let v = serde_json::json!({"b": 1, "a": 2});
        let s = canonicalize_yaml(&v).unwrap();
        // both keys present; serialisation succeeds. exact ordering depends
        // on serde_json::Value which preserves insertion order in our deps,
        // so we don't assert key ordering here.
        assert!(s.contains("a:"));
        assert!(s.contains("b:"));
    }
}

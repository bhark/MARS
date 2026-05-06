//! `${VAR}` and `${VAR:-default}` substitution over the YAML source string.
//!
//! Done before parse for simplicity. We always substitute, no quoting-aware
//! dance: keep the policy obvious. Operators who need a literal `$` in YAML
//! should use double-dollar escape `$$`, which we emit back as a single `$`.
//!
//! Substituted values are validated against per-value structural risks
//! (newlines, colon-space, space-hash, YAML tags, document separators).
//! Quote balance is intentionally not enforced per-value: legitimate values
//! like `a's` inside a double-quoted YAML scalar would otherwise be flagged
//! as unbalanced. The full substituted document is re-parsed by serde_yaml_ng
//! downstream, which catches any structural corruption that survives the
//! per-value gate.
//!
//! Nested defaults like `${A:-${B}}` are explicitly rejected: the current
//! regex cannot parse them safely and silent misparsing would be worse than
//! a clear error.

use std::env;
use std::sync::LazyLock;

use regex::Regex;

use crate::ConfigError;

#[allow(clippy::expect_used)]
static ENV_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\$|\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}").expect("env regex is valid"));

/// Apply env substitution to `src`. Unknown variables without a default
/// produce `EnvMissing`. Double-dollar `$$` is preserved as literal `$`.
/// Each substituted value is checked for characters that would break YAML
/// structure (colon-space, space-hash, newlines, YAML tags, doc separators).
pub(crate) fn substitute(src: &str) -> Result<String, ConfigError> {
    detect_nested_defaults(src)?;

    let mut missing: Option<String> = None;
    let mut out = String::with_capacity(src.len());
    let mut last_end = 0;

    for caps in ENV_RE.captures_iter(src) {
        let Some(m) = caps.get(0) else {
            continue;
        };
        out.push_str(&src[last_end..m.start()]);

        if m.as_str() == "$$" {
            out.push('$');
            last_end = m.end();
            continue;
        }

        let name = &caps[1];
        let default = caps.get(2).map(|m| m.as_str().to_string());
        let value = match env::var(name) {
            Ok(v) => v,
            Err(_) => match default {
                Some(d) => d,
                None => {
                    if missing.is_none() {
                        missing = Some(name.to_string());
                    }
                    String::new()
                }
            },
        };

        validate_yaml_safe(&value).map_err(|reason| ConfigError::Invalid(format!("env var {name}: {reason}")))?;

        out.push_str(&value);
        last_end = m.end();
    }
    out.push_str(&src[last_end..]);

    if let Some(name) = missing {
        return Err(ConfigError::EnvMissing(name));
    }
    Ok(out)
}

/// Reject `${A:-${B}}`-style nested defaults: the current regex cannot
/// parse them and silent misparse (treating `${A:-${B}` as the match) is
/// worse than a clear error. Done by scanning for `${` followed eventually
/// by another `${` before the matching `}`.
fn detect_nested_defaults(src: &str) -> Result<(), ConfigError> {
    let bytes = src.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        // skip the `$$` literal-dollar escape
        if bytes[i] == b'$' && bytes[i + 1] == b'$' {
            i += 2;
            continue;
        }
        if bytes[i] == b'$' && bytes[i + 1] == b'{' {
            let outer_start = i;
            // find first `}` and check whether another `${` appears before it
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'}' {
                if bytes[j] == b'$' && j + 1 < bytes.len() && bytes[j + 1] == b'{' {
                    let snippet_end = (outer_start + 32).min(bytes.len());
                    let snippet = String::from_utf8_lossy(&bytes[outer_start..snippet_end]);
                    return Err(ConfigError::Invalid(format!(
                        "nested ${{}} expansions are not supported (near {snippet:?}); flatten defaults to a single level"
                    )));
                }
                j += 1;
            }
            i = j.saturating_add(1);
            continue;
        }
        i += 1;
    }
    Ok(())
}

fn validate_yaml_safe(value: &str) -> Result<(), &'static str> {
    if value.contains('\n') || value.contains('\r') {
        return Err("contains newline");
    }
    if value.contains('\t') {
        return Err("contains tab");
    }
    // colon-space introduces a mapping in unquoted YAML scalars
    if value.contains(": ") {
        return Err("contains ': ' which is invalid in a YAML scalar");
    }
    // space-hash starts a comment in unquoted YAML scalars
    if value.contains(" #") {
        return Err("contains ' #' which is invalid in a YAML scalar");
    }
    // yaml tags and document separators can change semantics
    if value.contains("!!") {
        return Err("contains YAML tag indicator");
    }
    if value.contains("---") {
        return Err("contains YAML document separator");
    }
    // quote balance is intentionally not enforced: a value like `a's` is
    // legitimate inside a double-quoted yaml scalar, and the downstream
    // serde_yaml_ng parse catches any structural corruption that survives.
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_newline() {
        assert!(validate_yaml_safe("hello\nworld").is_err());
        assert!(validate_yaml_safe("hello\rworld").is_err());
    }

    #[test]
    fn validate_rejects_tab() {
        assert!(validate_yaml_safe("hello\tworld").is_err());
    }

    #[test]
    fn validate_rejects_colon_space() {
        assert!(validate_yaml_safe("foo: bar").is_err());
    }

    #[test]
    fn validate_rejects_space_hash() {
        assert!(validate_yaml_safe("foo #comment").is_err());
    }

    #[test]
    fn validate_rejects_yaml_tag() {
        assert!(validate_yaml_safe("!!str").is_err());
    }

    #[test]
    fn validate_rejects_doc_separator() {
        assert!(validate_yaml_safe("---").is_err());
    }

    #[test]
    fn validate_accepts_unbalanced_quotes() {
        // legitimate inside a double-quoted YAML scalar (`"a's"`).
        assert!(validate_yaml_safe("a's").is_ok());
        assert!(validate_yaml_safe("\"").is_ok());
        assert!(validate_yaml_safe("'").is_ok());
    }

    #[test]
    fn validate_accepts_balanced_quotes() {
        assert!(validate_yaml_safe("\"hello\"").is_ok());
        assert!(validate_yaml_safe("'hello'").is_ok());
    }

    #[test]
    fn validate_accepts_safe_specials() {
        assert!(validate_yaml_safe("foo:bar").is_ok()); // colon without space
        assert!(validate_yaml_safe("foo#bar").is_ok()); // hash without space
        assert!(validate_yaml_safe("$PATH").is_ok());
    }

    #[test]
    fn substitute_double_dollar_escape() {
        let result = substitute("cost is $$5").unwrap();
        assert_eq!(result, "cost is $5");
    }

    #[test]
    fn substitute_existing_var() {
        // PATH is virtually guaranteed to exist
        let result = substitute("path=${PATH}").unwrap();
        assert!(!result.contains("${PATH}"), "variable was not substituted");
    }

    #[test]
    fn substitute_default_when_unset() {
        // use a variable name that is extremely unlikely to exist
        let result = substitute("val=${MARS_TEST_XYZ_UNSET_VAR:-fallback}").unwrap();
        assert_eq!(result, "val=fallback");
    }

    #[test]
    fn substitute_missing_var_errors() {
        let err = substitute("val=${MARS_TEST_XYZ_UNSET_VAR_NO_DEFAULT}").unwrap_err();
        assert!(matches!(err, ConfigError::EnvMissing(name) if name == "MARS_TEST_XYZ_UNSET_VAR_NO_DEFAULT"));
    }

    #[test]
    fn substitute_multiple_tokens() {
        let result = substitute("a=${PATH} b=${PATH}").unwrap();
        assert_eq!(result.matches("${PATH}").count(), 0);
    }

    #[test]
    fn substitute_rejects_unsafe_value() {
        // a default containing a newline should be rejected
        let err = substitute("val=${MARS_X:-foo\nbar}").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("newline"), "expected newline rejection, got {msg}");
    }

    #[test]
    fn substitute_passes_through_apostrophe_in_double_quoted_context() {
        // simulates `key: "${VAR}"` with VAR=`a's` — must not be rejected.
        let result = substitute("key: \"${MARS_TEST_APOS:-a's}\"").unwrap();
        assert_eq!(result, "key: \"a's\"");
        // and the resulting yaml must parse cleanly downstream
        let v: serde_yaml_ng::Value = serde_yaml_ng::from_str(&result).unwrap();
        assert_eq!(v["key"].as_str(), Some("a's"));
    }

    #[test]
    fn substitute_rejects_nested_defaults() {
        // ${A:-${B}} would silently misparse with the `[^}]*` regex; reject it.
        let err = substitute("val=${A:-${B:-x}}").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("nested"), "expected nested-defaults rejection, got {msg}");
    }

    #[test]
    fn substitute_double_dollar_does_not_trigger_nested_check() {
        // `$$` is the escape for a literal `$`; must not be flagged as nested
        // even when followed by a `{...}` token.
        let result = substitute("a=$${BAR:-x}").unwrap();
        assert_eq!(result, "a=${BAR:-x}");
    }
}

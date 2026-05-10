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
use std::sync::OnceLock;

use regex::Regex;

use crate::ConfigError;

const ENV_RE_PATTERN: &str = r"\$\$|\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}";

/// Compile the env-substitution regex once, surfacing failures as a typed
/// error rather than panicking at module init. The literal is fine today, but
/// a future tweak that breaks compile would otherwise crash the process
/// before any logging is initialised.
fn env_re() -> Result<&'static Regex, ConfigError> {
    static CELL: OnceLock<Result<Regex, String>> = OnceLock::new();
    match CELL.get_or_init(|| Regex::new(ENV_RE_PATTERN).map_err(|e| e.to_string())) {
        Ok(r) => Ok(r),
        Err(msg) => Err(ConfigError::Invalid(format!("env regex compile failed: {msg}"))),
    }
}

/// Apply env substitution to `src`. Unknown variables without a default
/// produce `EnvMissing`. Double-dollar `$$` is preserved as literal `$`.
/// Each substituted value is checked for characters that would break YAML
/// structure (colon-space, space-hash, newlines, YAML tags, doc separators).
pub(crate) fn substitute(src: &str) -> Result<String, ConfigError> {
    detect_nested_defaults(src)?;

    let comment_mask = comment_mask(src);
    let mut missing: Option<String> = None;
    let mut out = String::with_capacity(src.len());
    let mut last_end = 0;

    for caps in env_re()?.captures_iter(src) {
        let Some(m) = caps.get(0) else {
            continue;
        };
        out.push_str(&src[last_end..m.start()]);

        // matches inside YAML comments are not config inputs; pass through
        // verbatim so a commented-out `${VAR}` does not produce EnvMissing.
        if comment_mask.get(m.start()).copied().unwrap_or(false) {
            out.push_str(m.as_str());
            last_end = m.end();
            continue;
        }

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

/// Build a byte-mask flagging positions that lie inside a YAML line comment.
/// A `#` starts a comment only when not inside a quoted scalar and either at
/// line start or preceded by whitespace. Quote state is tracked per line; YAML
/// scalars rarely span lines and any real structural corruption is caught by
/// the downstream parse.
fn comment_mask(src: &str) -> Vec<bool> {
    let bytes = src.as_bytes();
    let mut mask = vec![false; bytes.len()];
    let mut in_single = false;
    let mut in_double = false;
    let mut at_line_start = true;
    let mut prev_byte: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' {
            in_single = false;
            in_double = false;
            at_line_start = true;
            prev_byte = None;
            i += 1;
            continue;
        }
        if !in_single && !in_double {
            let after_ws = prev_byte.is_none_or(|p| p == b' ' || p == b'\t');
            if b == b'#' && (at_line_start || after_ws) {
                let mut j = i;
                while j < bytes.len() && bytes[j] != b'\n' {
                    mask[j] = true;
                    j += 1;
                }
                prev_byte = Some(b'#');
                at_line_start = false;
                i = j;
                continue;
            }
            if b == b'\'' {
                in_single = true;
            } else if b == b'"' {
                in_double = true;
            }
        } else if in_single && b == b'\'' {
            in_single = false;
        } else if in_double && b == b'"' && prev_byte != Some(b'\\') {
            in_double = false;
        }
        if b != b' ' && b != b'\t' {
            at_line_start = false;
        }
        prev_byte = Some(b);
        i += 1;
    }
    mask
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
        // simulates `key: "${VAR}"` with VAR=`a's` - must not be rejected.
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
    fn substitute_skips_var_in_full_line_comment() {
        let result = substitute("# disabled: ${MARS_TEST_NEVER_SET}\nkey: val\n").unwrap();
        assert_eq!(result, "# disabled: ${MARS_TEST_NEVER_SET}\nkey: val\n");
    }

    #[test]
    fn substitute_skips_var_in_trailing_comment() {
        let result = substitute("key: val # ${MARS_TEST_NEVER_SET}\n").unwrap();
        assert_eq!(result, "key: val # ${MARS_TEST_NEVER_SET}\n");
    }

    #[test]
    fn substitute_first_var_substitutes_second_in_comment_stays() {
        // PATH always exists; OTHER_NEVER_SET would error if not in comment.
        let result = substitute("path=${PATH} # ${MARS_TEST_OTHER_NEVER_SET}\n").unwrap();
        assert!(!result.contains("${PATH}"));
        assert!(result.contains("${MARS_TEST_OTHER_NEVER_SET}"));
    }

    #[test]
    fn substitute_in_quoted_string_with_hash_is_not_a_comment() {
        let result = substitute("key: \"value # ${MARS_TEST_QUOTED:-fallback}\"\n").unwrap();
        assert_eq!(result, "key: \"value # fallback\"\n");
    }

    #[test]
    fn substitute_hash_without_leading_whitespace_is_not_a_comment() {
        // foo#${VAR} - the # is not preceded by whitespace, so not a comment
        let result = substitute("key: foo#${MARS_TEST_INLINE_HASH:-bar}\n").unwrap();
        assert_eq!(result, "key: foo#bar\n");
    }

    #[test]
    fn substitute_double_dollar_does_not_trigger_nested_check() {
        // `$$` is the escape for a literal `$`; must not be flagged as nested
        // even when followed by a `{...}` token.
        let result = substitute("a=$${BAR:-x}").unwrap();
        assert_eq!(result, "a=${BAR:-x}");
    }
}

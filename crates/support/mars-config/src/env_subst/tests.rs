#![allow(clippy::unwrap_used, clippy::panic)]

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

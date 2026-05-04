//! `${VAR}` and `${VAR:-default}` substitution over the YAML source string.
//!
//! Done before parse for simplicity. We always substitute, no quoting-aware
//! dance: keep the policy obvious. Operators who need a literal `$` in YAML
//! should use double-dollar escape `$$`, which we emit back as a single `$`.
//!
//! Substituted values are validated for YAML scalar safety: newlines,
//! colon-space, space-hash, and unbalanced quotes are rejected so that a
//! substituted value cannot corrupt the document structure or change its
//! semantics.

use std::env;
use std::sync::LazyLock;

use regex::Regex;

use crate::ConfigError;

static ENV_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\$|\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}")
        .expect("env regex is valid")
});

/// Apply env substitution to `src`. Unknown variables without a default
/// produce `EnvMissing`. Double-dollar `$$` is preserved as literal `$`.
/// Each substituted value is checked for characters that would break YAML
/// structure (colon-space, space-hash, newlines, unbalanced quotes).
pub(crate) fn substitute(src: &str) -> Result<String, ConfigError> {

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
    // unbalanced quotes could break a quoted string context in the template
    let double_quotes = value.chars().filter(|&c| c == '"').count();
    let single_quotes = value.chars().filter(|&c| c == '\'').count();
    if double_quotes % 2 != 0 {
        return Err("contains unbalanced double quotes");
    }
    if single_quotes % 2 != 0 {
        return Err("contains unbalanced single quotes");
    }
    Ok(())
}

// inline tests omitted: env mutation is unsafe in edition 2024 and the crate
// forbids unsafe. behavior is exercised end-to-end in tests/loader.rs.

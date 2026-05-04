//! `${VAR}` and `${VAR:-default}` substitution over the YAML source string.
//!
//! Done before parse for simplicity. We always substitute, no quoting-aware
//! dance: keep the policy obvious. Operators who need a literal `$` in YAML
//! should use double-dollar escape `$$`, which we emit back as a single `$`.

use std::env;

use regex::{Captures, Regex};

use crate::ConfigError;

/// Apply env substitution to `src`. Unknown variables without a default
/// produce `EnvMissing`. Double-dollar `$$` is preserved as literal `$`.
pub(crate) fn substitute(src: &str) -> Result<String, ConfigError> {
    // placeholder: ${NAME} or ${NAME:-default}. names are POSIX-ish.
    let re = Regex::new(r"\$\$|\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}")
        .map_err(|e| ConfigError::Parse(format!("env regex: {e}")))?;

    let mut missing: Option<String> = None;
    let out = re.replace_all(src, |caps: &Captures<'_>| {
        if &caps[0] == "$$" {
            return "$".to_string();
        }
        let name = &caps[1];
        let default: Option<String> = caps.get(2).map(|m| m.as_str().to_string());
        match env::var(name) {
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
        }
    });

    if let Some(name) = missing {
        return Err(ConfigError::EnvMissing(name));
    }
    Ok(out.into_owned())
}

// inline tests omitted: env mutation is unsafe in edition 2024 and the crate
// forbids unsafe. behavior is exercised end-to-end in tests/loader.rs.

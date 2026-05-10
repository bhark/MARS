//! Unit-suffixed scalar parsing.
//!
//! Sizes use IEC binary suffixes (`KiB`, `MiB`, `GiB`, `TiB`) and the SI
//! decimal forms (`KB`, `MB`, `GB`, `TB`). Distances are metres. Durations
//! defer to the `humantime` crate.

use std::time::Duration;

use crate::ConfigError;

/// Parse a byte-size literal like `12.5KiB`, `50GiB`, `1024B`, `2MB`.
pub fn parse_bytes(input: &str) -> Result<u64, ConfigError> {
    let s = input.trim();
    let (num, unit) = split_unit(s);
    let value: f64 = num
        .parse()
        .map_err(|_| ConfigError::Invalid(format!("invalid size number in {input:?}")))?;
    if value < 0.0 {
        return Err(ConfigError::Invalid(format!("negative size: {input}")));
    }
    let mult: f64 = match unit {
        "" | "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0_f64.powi(3),
        "TiB" => 1024.0_f64.powi(4),
        "KB" | "kB" => 1000.0,
        "MB" => 1_000_000.0,
        "GB" => 1_000_000_000.0,
        "TB" => 1_000_000_000_000.0,
        other => {
            return Err(ConfigError::Invalid(format!(
                "unknown size unit {other:?} in {input:?}"
            )));
        }
    };
    let bytes = value * mult;
    if !bytes.is_finite() || bytes > (u64::MAX as f64) {
        return Err(ConfigError::Invalid(format!("size out of range: {input}")));
    }
    Ok(bytes.round() as u64)
}

/// Parse a metric distance like `4096m`, `2.5km`. Returns metres.
pub fn parse_distance_m(input: &str) -> Result<f64, ConfigError> {
    let s = input.trim();
    let (num, unit) = split_unit(s);
    let value: f64 = num
        .parse()
        .map_err(|_| ConfigError::Invalid(format!("invalid distance in {input:?}")))?;
    let mult = match unit {
        "m" => 1.0,
        "km" => 1000.0,
        "cm" => 0.01,
        "mm" => 0.001,
        other => {
            return Err(ConfigError::Invalid(format!(
                "unknown distance unit {other:?} in {input:?}"
            )));
        }
    };
    Ok(value * mult)
}

/// Parse a duration via `humantime`. Accepts `5min`, `30s`, `1h30min`, etc.
pub fn parse_duration(input: &str) -> Result<Duration, ConfigError> {
    humantime::parse_duration(input.trim())
        .map_err(|e| ConfigError::Invalid(format!("invalid duration {input:?}: {e}")))
}

// split into leading numeric chunk and trailing unit chunk.
fn split_unit(s: &str) -> (&str, &str) {
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    (num.trim(), unit.trim())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn bytes_kib_mib_gib() {
        assert_eq!(parse_bytes("1KiB").unwrap(), 1024);
        assert_eq!(parse_bytes("12.5KiB").unwrap(), 12_800);
        assert_eq!(parse_bytes("1MiB").unwrap(), 1024 * 1024);
        assert_eq!(parse_bytes("50GiB").unwrap(), 50u64 * 1024 * 1024 * 1024);
        assert_eq!(parse_bytes("100").unwrap(), 100);
    }

    #[test]
    fn distance_m_km() {
        assert!((parse_distance_m("4096m").unwrap() - 4096.0).abs() < f64::EPSILON);
        assert!((parse_distance_m("2.5km").unwrap() - 2500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn duration_min_s() {
        assert_eq!(parse_duration("5min").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
    }

    #[test]
    fn rejects_bad_unit() {
        assert!(parse_bytes("12foo").is_err());
        assert!(parse_distance_m("12foo").is_err());
        assert!(parse_duration("nope").is_err());
    }
}

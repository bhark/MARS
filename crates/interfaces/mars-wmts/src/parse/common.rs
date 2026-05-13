//! Shared KVP-parsing helpers used by every WMTS operation.
//!
//! KVP semantics: parameter names are case-insensitive (lowercased on parse,
//! per OGC 07-057r7 §8); values are preserved as-is. Repeated keys follow
//! last-win semantics - the spec does not pin a behaviour, so this is an
//! adapter choice that matches common WMTS server practice.

use std::collections::HashMap;

use percent_encoding::percent_decode_str;

use mars_types::ImageFormat;

use crate::WmtsError;

pub(super) type Kvp = HashMap<String, String>;

pub(super) fn parse_kvp(query: &str) -> Kvp {
    let mut out = HashMap::new();
    for pair in query.trim_start_matches('?').split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(k.to_ascii_lowercase(), pct_decode(v));
    }
    out
}

/// percent-decode a KVP value with `+` -> space (form-style). invalid escapes
/// pass through literally, matching the prior hand-rolled behaviour.
fn pct_decode(s: &str) -> String {
    let plus_decoded: String = s.chars().map(|c| if c == '+' { ' ' } else { c }).collect();
    percent_decode_str(&plus_decoded).decode_utf8_lossy().into_owned()
}

pub(super) fn require(kvp: &Kvp, name: &'static str) -> Result<String, WmtsError> {
    kvp.get(name)
        .filter(|s| !s.is_empty())
        .cloned()
        .ok_or(WmtsError::MissingParam(name))
}

pub(super) fn parse_u32(kvp: &Kvp, name: &'static str) -> Result<u32, WmtsError> {
    let raw = require(kvp, name)?;
    raw.parse()
        .map_err(|e: std::num::ParseIntError| WmtsError::InvalidParam {
            name,
            reason: e.to_string(),
        })
}

pub(super) fn parse_format(raw: &str) -> Result<ImageFormat, WmtsError> {
    match raw {
        "image/png" => Ok(ImageFormat::Png),
        "image/jpeg" | "image/jpg" => Ok(ImageFormat::Jpeg),
        other => Err(WmtsError::InvalidParam {
            name: "format",
            reason: format!("unsupported {other}"),
        }),
    }
}

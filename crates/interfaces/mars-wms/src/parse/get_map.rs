//! `GetMap` KVP extraction. Produces an Option-heavy [`ParsedGetMap`]
//! consumed by [`crate::prepare::resolve_get_map`]; this layer only does
//! tokenisation and shape parsing (u32, f64) - all semantic validation
//! (allowlists, bounds, axis-aware bbox, defaults) lives in prepare.

use mars_types::LayerId;

use super::common::{Kvp, nonempty, parse_kvp, parse_optional_u32};
use crate::prepare::viewport::ParsedViewport;
use crate::prepare::{ParsedGetMap, ResolvedGetMap, resolve_get_map};
use crate::{WmsConfig, WmsError, WmsVersion};

/// Parse a `GetMap` query-string and resolve it. Public facade used by the
/// dispatcher; tests; bins. Returns the runtime `RenderPlan` directly for
/// callers that don't care about EXCEPTIONS. Defaults to WMS 1.3.0 semantics
/// for backward compatibility with callers that haven't yet threaded a
/// negotiated [`WmsVersion`] in.
pub fn parse_get_map(query: &str, cfg: &WmsConfig) -> Result<mars_runtime::RenderPlan, WmsError> {
    let kvp = parse_kvp(query);
    let version = super::version::negotiate_version(&kvp)?;
    let parsed = parse_get_map_kvp(&kvp, version)?;
    Ok(resolve_get_map(parsed, cfg, version)?.plan)
}

/// Parse + resolve in one step; used by the dispatcher when it needs both
/// the plan and EXCEPTIONS.
pub(super) fn resolve_get_map_from_kvp(
    kvp: &Kvp,
    cfg: &WmsConfig,
    version: WmsVersion,
) -> Result<ResolvedGetMap, WmsError> {
    let parsed = parse_get_map_kvp(kvp, version)?;
    resolve_get_map(parsed, cfg, version)
}

/// KVP -> [`ParsedGetMap`]. Only fails on shape errors (e.g. `width=abc`
/// not a u32). Required-field and allowlist checks happen in prepare.
fn parse_get_map_kvp(kvp: &Kvp, version: WmsVersion) -> Result<ParsedGetMap, WmsError> {
    Ok(ParsedGetMap {
        viewport: parse_viewport(kvp, version)?,
        exceptions: nonempty(kvp, "exceptions"),
    })
}

/// Shared viewport-KVP extractor used by GetMap (here) and GetFeatureInfo.
/// Reads the version-appropriate CRS key (`crs` for 1.3.0, `srs` for 1.1.1)
/// so the downstream prepare layer sees a single normalised field.
pub(crate) fn parse_viewport(kvp: &Kvp, version: WmsVersion) -> Result<ParsedViewport, WmsError> {
    Ok(ParsedViewport {
        layers: parse_layers(kvp),
        crs: parse_crs(kvp, version),
        bbox: nonempty(kvp, "bbox"),
        width: parse_optional_u32(kvp, "width")?,
        height: parse_optional_u32(kvp, "height")?,
        format: nonempty(kvp, "format"),
        dpi: parse_optional_dpi(kvp)?,
    })
}

/// 1.1.1 used `SRS=`; 1.3.0 uses `CRS=`. Be permissive when both are
/// supplied: prefer the version-correct key, fall back to the other so
/// mildly malformed clients still get through.
fn parse_crs(kvp: &Kvp, version: WmsVersion) -> Option<String> {
    let (primary, fallback) = match version {
        WmsVersion::V111 => ("srs", "crs"),
        WmsVersion::V130 => ("crs", "srs"),
    };
    nonempty(kvp, primary).or_else(|| nonempty(kvp, fallback))
}

fn parse_layers(kvp: &Kvp) -> Option<Vec<LayerId>> {
    let raw = kvp.get("layers").filter(|s| !s.is_empty())?;
    Some(raw.split(',').filter(|s| !s.is_empty()).map(LayerId::new).collect())
}

fn parse_optional_dpi(kvp: &Kvp) -> Result<Option<f64>, WmsError> {
    let raw = match kvp.get("dpi").or_else(|| kvp.get("map_resolution")) {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(None),
    };
    let dpi: f64 = raw
        .parse()
        .map_err(|e: std::num::ParseFloatError| WmsError::InvalidParam {
            name: "dpi",
            reason: e.to_string(),
        })?;
    Ok(Some(dpi))
}

#[cfg(test)]
mod tests;

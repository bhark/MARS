//! `GetFeatureInfo` KVP extraction. Produces an Option-heavy
//! [`ParsedGetFeatureInfo`] consumed by
//! [`crate::prepare::resolve_get_feature_info`]; reuses
//! [`super::get_map::parse_viewport`] for the shared LAYERS/CRS/BBOX/...
//! slice so the two ops share one extractor and one validator.

use mars_types::LayerId;

use super::common::{Kvp, nonempty, parse_kvp, parse_optional_u32};
use super::get_map::parse_viewport;
use super::version::negotiate_version;
use crate::prepare::{ParsedGetFeatureInfo, ResolvedGetFeatureInfo, resolve_get_feature_info};
use crate::{WmsConfig, WmsError, WmsVersion};

/// Parse a `GetFeatureInfo` query-string into a [`ResolvedGetFeatureInfo`].
pub fn parse_get_feature_info(query: &str, cfg: &WmsConfig) -> Result<ResolvedGetFeatureInfo, WmsError> {
    let kvp = parse_kvp(query);
    let version = negotiate_version(&kvp)?;
    let parsed = parse_get_feature_info_kvp(&kvp, version)?;
    resolve_get_feature_info(parsed, cfg, version)
}

pub(super) fn resolve_get_feature_info_from_kvp(
    kvp: &Kvp,
    cfg: &WmsConfig,
    version: WmsVersion,
) -> Result<ResolvedGetFeatureInfo, WmsError> {
    let parsed = parse_get_feature_info_kvp(kvp, version)?;
    resolve_get_feature_info(parsed, cfg, version)
}

fn parse_get_feature_info_kvp(kvp: &Kvp, version: WmsVersion) -> Result<ParsedGetFeatureInfo, WmsError> {
    let (i_key, j_key, i_fallback, j_fallback) = match version {
        // 1.3.0 uses I/J; allow legacy X/Y as a fallback so clients that
        // mix conventions still work.
        WmsVersion::V130 => ("i", "j", "x", "y"),
        // 1.1.1 uses X/Y; allow I/J as a fallback for the same reason.
        WmsVersion::V111 => ("x", "y", "i", "j"),
    };
    let i = parse_optional_u32(kvp, i_key)?.or(parse_optional_u32(kvp, i_fallback)?);
    let j = parse_optional_u32(kvp, j_key)?.or(parse_optional_u32(kvp, j_fallback)?);
    Ok(ParsedGetFeatureInfo {
        viewport: parse_viewport(kvp, version)?,
        query_layers: parse_query_layers(kvp),
        i,
        j,
        info_format: nonempty(kvp, "info_format"),
        feature_count: parse_optional_u32(kvp, "feature_count")?,
    })
}

fn parse_query_layers(kvp: &Kvp) -> Option<Vec<LayerId>> {
    let raw = kvp.get("query_layers").filter(|s| !s.is_empty())?;
    Some(raw.split(',').filter(|s| !s.is_empty()).map(LayerId::new).collect())
}

#[cfg(test)]
mod tests;

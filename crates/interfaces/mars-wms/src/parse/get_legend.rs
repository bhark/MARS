//! `GetLegendGraphic` KVP extraction. Produces an Option-heavy
//! [`ParsedGetLegend`] consumed by [`crate::prepare::resolve_get_legend`].

use super::common::{nonempty, parse_kvp, parse_optional_u32, Kvp};
use crate::prepare::{resolve_get_legend, ParsedGetLegend, ResolvedGetLegend};
use crate::{WmsConfig, WmsError};

/// Parse a `GetLegendGraphic` query-string into a [`ResolvedGetLegend`].
pub fn parse_get_legend_graphic(query: &str, cfg: &WmsConfig) -> Result<ResolvedGetLegend, WmsError> {
    let kvp = parse_kvp(query);
    let parsed = parse_get_legend_kvp(&kvp)?;
    resolve_get_legend(parsed, cfg)
}

pub(super) fn resolve_get_legend_from_kvp(kvp: &Kvp, cfg: &WmsConfig) -> Result<ResolvedGetLegend, WmsError> {
    let parsed = parse_get_legend_kvp(kvp)?;
    resolve_get_legend(parsed, cfg)
}

fn parse_get_legend_kvp(kvp: &Kvp) -> Result<ParsedGetLegend, WmsError> {
    Ok(ParsedGetLegend {
        layer: nonempty(kvp, "layer"),
        format: nonempty(kvp, "format"),
        width: parse_optional_u32(kvp, "width")?,
        height: parse_optional_u32(kvp, "height")?,
        rule: nonempty(kvp, "rule"),
    })
}

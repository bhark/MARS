//! WMTS `GetTile` extraction for both the KVP transport (`/wmts?...`) and
//! the REST resource path
//! (`/wmts/{Layer}/{Style}/{TileMatrixSet}/{TileMatrix}/{TileRow}/{TileCol}.{ext}`).
//!
//! Both transports lower to a single [`ParsedGetTile`] shape; semantic
//! validation and bbox math live in [`crate::prepare::resolve_get_tile`].
//! That single chokepoint guarantees REST and KVP cache keys never drift.

use mars_runtime::RenderPlan;
use mars_types::ImageFormat;

use mars_ows_common::nonempty;

use super::common::{Kvp, parse_kvp, parse_optional_u32};
use crate::prepare::{ParsedGetTile, ResolvedGetTile, resolve_get_tile};
use crate::{WmtsConfig, WmtsError};

/// Parse a KVP `GetTile` query-string into a [`RenderPlan`].
pub fn parse_get_tile(query: &str, cfg: &WmtsConfig) -> Result<RenderPlan, WmtsError> {
    let kvp = parse_kvp(query);
    Ok(resolve_get_tile_from_kvp(&kvp, cfg)?.plan)
}

pub(super) fn resolve_get_tile_from_kvp(kvp: &Kvp, cfg: &WmtsConfig) -> Result<ResolvedGetTile, WmtsError> {
    let parsed = parse_kvp_get_tile(kvp)?;
    resolve_get_tile(parsed, cfg)
}

fn parse_kvp_get_tile(kvp: &Kvp) -> Result<ParsedGetTile, WmtsError> {
    Ok(ParsedGetTile {
        version: nonempty(kvp, "version"),
        layer: nonempty(kvp, "layer"),
        format: nonempty(kvp, "format").map(|s| parse_format_mime(&s)).transpose()?,
        tilematrixset: nonempty(kvp, "tilematrixset"),
        tilematrix: nonempty(kvp, "tilematrix"),
        tilecol: parse_optional_u32(kvp, "tilecol")?,
        tilerow: parse_optional_u32(kvp, "tilerow")?,
    })
}

/// Parse a REST-form tile request. The router strips the path prefix and
/// hands `layer/style/tms/z/y/x` plus the file extension; `ext` is the suffix
/// after the final `.` (e.g. `png`, `jpg`, `jpeg`).
///
/// `version` cannot be carried in the REST path - WMTS 1.0.0 is implicit.
/// `style` per spec may be the literal `default` to mean "no style filter";
/// that distinction is collapsed to empty here.
#[allow(clippy::too_many_arguments)]
pub fn parse_rest_get_tile(
    layer: &str,
    style: &str,
    tms: &str,
    z: &str,
    y: &str,
    x: &str,
    ext: &str,
    cfg: &WmtsConfig,
) -> Result<RenderPlan, WmtsError> {
    let parsed = parse_rest(layer, style, tms, z, y, x, ext)?;
    Ok(resolve_get_tile(parsed, cfg)?.plan)
}

fn parse_rest(
    layer: &str,
    _style: &str,
    tms: &str,
    z: &str,
    y: &str,
    x: &str,
    ext: &str,
) -> Result<ParsedGetTile, WmtsError> {
    let format = parse_format_ext(ext)?;
    let tile_col: u32 = x
        .parse()
        .map_err(|e: std::num::ParseIntError| WmtsError::InvalidParam {
            name: "tilecol",
            reason: e.to_string(),
        })?;
    let tile_row: u32 = y
        .parse()
        .map_err(|e: std::num::ParseIntError| WmtsError::InvalidParam {
            name: "tilerow",
            reason: e.to_string(),
        })?;
    // `style` (KVP or REST) is intentionally discarded: the renderer does
    // not yet route per-style. Restore the field once there's a consumer.
    Ok(ParsedGetTile {
        version: None,
        layer: Some(layer.to_owned()),
        format: Some(format),
        tilematrixset: Some(tms.to_owned()),
        tilematrix: Some(z.to_owned()),
        tilecol: Some(tile_col),
        tilerow: Some(tile_row),
    })
}

fn parse_format_mime(raw: &str) -> Result<ImageFormat, WmtsError> {
    ImageFormat::from_mime(raw).ok_or_else(|| WmtsError::InvalidParam {
        name: "format",
        reason: format!("unsupported {raw}"),
    })
}

/// Map a REST URL file extension to an [`ImageFormat`].
fn parse_format_ext(ext: &str) -> Result<ImageFormat, WmtsError> {
    ImageFormat::from_extension(ext).ok_or_else(|| WmtsError::InvalidParam {
        name: "format",
        reason: format!("unsupported extension `.{ext}`"),
    })
}

#[cfg(test)]
mod tests;

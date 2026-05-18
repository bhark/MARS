//! WMTS 1.0.0 request parsing.
//!
//! Covers both `GetTile` and `GetCapabilities` on the KVP path
//! (`/wmts?...`) and `GetTile` on the REST resource path
//! (`/wmts/{Layer}/{Style}/{TileMatrixSet}/{TileMatrix}/{TileRow}/{TileCol}.{ext}`).
//! `GetFeatureInfo` rejects with `WmtsError::NotImplemented`.
//!
//! Per-operation parsing lives in [`get_tile`]; shared KVP helpers live in
//! [`common`].

pub(crate) mod common;
pub(crate) mod get_tile;

use common::{parse_kvp, require};
use get_tile::resolve_get_tile_from_kvp;

pub use get_tile::{parse_get_tile, parse_rest_get_tile};

use crate::{WmtsConfig, WmtsError, WmtsRequest};

/// Parse any WMTS request, dispatching on the `request=` parameter.
pub fn parse_request(query: &str, cfg: &WmtsConfig) -> Result<WmtsRequest, WmtsError> {
    let kvp = parse_kvp(query);
    let request = require(&kvp, "request")?;
    match request.as_str() {
        s if s.eq_ignore_ascii_case("GetTile") => Ok(WmtsRequest::GetTile(resolve_get_tile_from_kvp(&kvp, cfg)?)),
        s if s.eq_ignore_ascii_case("GetCapabilities") => Ok(WmtsRequest::GetCapabilities),
        other => Err(WmtsError::NotImplemented {
            what: format!("WMTS request={other}"),
        }),
    }
}

#[cfg(test)]
mod tests;

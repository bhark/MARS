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
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeMap;

    use mars_config::{TileMatrixLevel, TileMatrixSet};
    use mars_types::{CrsCode, ImageFormat};

    use super::*;

    fn cfg() -> WmtsConfig {
        let mut sets = BTreeMap::new();
        sets.insert(
            "dk_25832".to_owned(),
            TileMatrixSet {
                crs: CrsCode::new("EPSG:25832"),
                top_left: [120_000.0, 6_500_000.0],
                tile_size: [256, 256],
                levels: vec![TileMatrixLevel {
                    id: 0,
                    scale_denominator: 1638.4 / 0.000_28,
                    matrix_width: 1,
                    matrix_height: 1,
                }],
            },
        );
        WmtsConfig {
            tile_matrix_sets: sets,
            formats: vec![ImageFormat::Png],
            max_bbox_coord: 1e9,
            layer_policies: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn dispatch_capabilities() {
        let q = "service=WMTS&version=1.0.0&request=GetCapabilities";
        let req = parse_request(q, &cfg()).unwrap();
        assert!(matches!(req, WmtsRequest::GetCapabilities));
    }

    #[test]
    fn unknown_request_not_implemented() {
        let q = "request=GetFeatureInfo&layer=a&format=image/png&tilematrixset=dk_25832&\
                 tilematrix=0&tilecol=0&tilerow=0&i=1&j=1";
        let err = parse_request(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmtsError::NotImplemented { .. }));
    }
}

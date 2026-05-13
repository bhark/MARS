//! WMS 1.3.0 KVP request parsing.
//!
//! Covers `GetMap`, `GetCapabilities`, `GetFeatureInfo`, and
//! `GetLegendGraphic`. SLD-related requests reject with
//! `WmsError::NotImplemented` so they round-trip to an XML exception in
//! the edge.
//!
//! Per-operation parsing lives in [`get_map`], [`get_feature_info`], and
//! [`get_legend`]; shared KVP helpers live in [`common`].

pub(crate) mod common;
mod get_feature_info;
mod get_legend;
pub(crate) mod get_map;

use common::{parse_kvp, require};
use get_feature_info::resolve_get_feature_info_from_kvp;
use get_legend::parse_get_legend_graphic_inner;
use get_map::resolve_get_map_from_kvp;

pub use get_feature_info::parse_get_feature_info;
pub use get_legend::parse_get_legend_graphic;
pub use get_map::parse_get_map;

use crate::{WmsConfig, WmsError, WmsRequest};

/// Parse any WMS request, dispatching on the `request=` parameter.
pub fn parse_request(query: &str, cfg: &WmsConfig) -> Result<WmsRequest, WmsError> {
    let kvp = parse_kvp(query);
    let request = require(&kvp, "request")?;
    match request.as_str() {
        s if s.eq_ignore_ascii_case("GetMap") => Ok(WmsRequest::GetMap(resolve_get_map_from_kvp(&kvp, cfg)?)),
        s if s.eq_ignore_ascii_case("GetCapabilities") => Ok(WmsRequest::GetCapabilities),
        s if s.eq_ignore_ascii_case("GetFeatureInfo") => {
            Ok(WmsRequest::GetFeatureInfo(resolve_get_feature_info_from_kvp(&kvp, cfg)?))
        }
        s if s.eq_ignore_ascii_case("GetLegendGraphic") => {
            Ok(WmsRequest::GetLegendGraphic(parse_get_legend_graphic_inner(&kvp, cfg)?))
        }
        other => Err(WmsError::NotImplemented {
            what: format!("WMS request={other}"),
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use mars_types::{CrsCode, ImageFormat};

    use super::*;

    fn cfg() -> WmsConfig {
        WmsConfig {
            allowlist_crs: vec![CrsCode::new("EPSG:25832"), CrsCode::new("EPSG:4326")],
            formats: vec![ImageFormat::Png],
            max_image_dimension: 8192,
            max_pixels: 16_000_000,
            max_layers: 100,
            max_bbox_coord: 1e9,
            scale_pixel_size_m: 0.0254 / 96.0,
        }
    }

    #[test]
    fn dispatch_capabilities() {
        let q = "service=WMS&version=1.3.0&request=GetCapabilities";
        let req = parse_request(q, &cfg()).unwrap();
        assert!(matches!(req, WmsRequest::GetCapabilities));
    }
}

//! WMS KVP request parsing.
//!
//! Covers `GetMap`, `GetCapabilities`, `GetFeatureInfo`, and
//! `GetLegendGraphic`. SLD-related requests reject with
//! `WmsError::NotImplemented` so they round-trip to an XML exception in
//! the edge.
//!
//! Per-operation parsing lives in [`get_map`], [`get_feature_info`], and
//! [`get_legend`]; shared KVP helpers live in [`common`]. Version
//! negotiation lives in [`version`] and runs before dispatch so the
//! handler can format error responses in the requested protocol version.

pub(crate) mod common;
mod get_feature_info;
mod get_legend;
pub(crate) mod get_map;
mod version;

use common::{parse_kvp, require};
use get_feature_info::resolve_get_feature_info_from_kvp;
use get_legend::resolve_get_legend_from_kvp;
use get_map::resolve_get_map_from_kvp;
use version::negotiate_version;

pub use get_feature_info::parse_get_feature_info;
pub use get_legend::parse_get_legend_graphic;
pub use get_map::parse_get_map;
pub use version::version_for_error_response;

use crate::{WmsConfig, WmsError, WmsRequest, WmsVersion};

/// Parse any WMS request, dispatching on the `request=` parameter. Version
/// is negotiated first so subsequent error paths can format
/// `ServiceExceptionReport` documents in the requested protocol version.
pub fn parse_request(query: &str, cfg: &WmsConfig) -> Result<(WmsVersion, WmsRequest), WmsError> {
    let kvp = parse_kvp(query);
    let version = negotiate_version(&kvp)?;
    let request = require(&kvp, "request")?;
    let request = match request.as_str() {
        s if s.eq_ignore_ascii_case("GetMap") => WmsRequest::GetMap(resolve_get_map_from_kvp(&kvp, cfg, version)?),
        s if s.eq_ignore_ascii_case("GetCapabilities") => WmsRequest::GetCapabilities,
        s if s.eq_ignore_ascii_case("GetFeatureInfo") => {
            WmsRequest::GetFeatureInfo(resolve_get_feature_info_from_kvp(&kvp, cfg, version)?)
        }
        s if s.eq_ignore_ascii_case("GetLegendGraphic") => {
            WmsRequest::GetLegendGraphic(resolve_get_legend_from_kvp(&kvp, cfg)?)
        }
        other => {
            return Err(WmsError::NotImplemented {
                what: format!("WMS request={other}"),
            });
        }
    };
    Ok((version, request))
}

#[cfg(test)]
mod tests;

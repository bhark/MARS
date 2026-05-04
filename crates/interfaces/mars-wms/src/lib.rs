//! WMS 1.3.0 interface adapter.
//!
//! Translates WMS request parameters into a `mars_runtime::RenderPlan` and
//! generates the capabilities document. SPEC §12 - full GetMap, GetFeatureInfo,
//! GetLegendGraphic. Out of scope for v1: SLD / SLD_BODY, DescribeLayer.

#![forbid(unsafe_code)]

use mars_runtime::RenderPlan;

#[derive(Debug, thiserror::Error)]
pub enum WmsError {
    #[error("missing parameter: {0}")]
    MissingParam(&'static str),
    #[error("invalid parameter `{name}`: {reason}")]
    InvalidParam { name: &'static str, reason: String },
    #[error("not implemented: {what}")]
    NotImplemented { what: &'static str },
}

/// Parse a `GetMap` query-string into a `RenderPlan`. Phase 0 stub.
pub fn parse_get_map(_query: &str) -> Result<RenderPlan, WmsError> {
    Err(WmsError::NotImplemented {
        what: "mars-wms::parse_get_map",
    })
}

/// Render the WMS `GetCapabilities` XML document. Phase 0 stub.
pub fn capabilities_xml() -> Result<String, WmsError> {
    Err(WmsError::NotImplemented {
        what: "mars-wms::capabilities_xml",
    })
}

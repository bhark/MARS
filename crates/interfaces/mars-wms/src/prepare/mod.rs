//! WMS request-side prepare layer.
//!
//! Mirrors `mars-render/src/prepare.rs`: the parse layer extracts the
//! Option-heavy `Parsed*` shape from KVP; this layer normalises it into a
//! validated `Resolved*` with every default unwrapped and every check
//! applied exactly once. The dispatcher in [`crate::parse::parse_request`]
//! composes the two and wraps the result in a [`crate::WmsRequest`] variant.
//!
//! Per-operation resolvers live in [`get_map`]; shared viewport
//! normalisation (LAYERS/CRS/BBOX/WIDTH/HEIGHT/FORMAT/DPI) lives in
//! [`viewport`].

pub(crate) mod get_feature_info;
pub(crate) mod get_map;
pub(crate) mod viewport;

pub use get_feature_info::ResolvedGetFeatureInfo;
pub use get_map::ResolvedGetMap;

pub(crate) use get_feature_info::resolve_get_feature_info;
pub(crate) use get_map::resolve_get_map;

use mars_types::LayerId;

use viewport::ParsedViewport;

/// Option-heavy GetMap shape produced by [`crate::parse::get_map`].
#[derive(Debug, Default, Clone)]
pub(crate) struct ParsedGetMap {
    pub viewport: ParsedViewport,
    pub exceptions: Option<String>,
}

/// Option-heavy GetFeatureInfo shape produced by
/// [`crate::parse::get_feature_info`].
#[derive(Debug, Default, Clone)]
pub(crate) struct ParsedGetFeatureInfo {
    pub viewport: ParsedViewport,
    pub query_layers: Option<Vec<LayerId>>,
    pub i: Option<u32>,
    pub j: Option<u32>,
    pub info_format: Option<String>,
    pub feature_count: Option<u32>,
}

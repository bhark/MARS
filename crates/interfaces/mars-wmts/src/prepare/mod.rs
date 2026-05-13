//! WMTS request-side prepare layer.
//!
//! Mirrors `mars-wms`'s prepare module and `mars-render/src/prepare.rs`:
//! the parse layer extracts the Option-heavy `Parsed*` shape from KVP or
//! REST inputs; this layer normalises it into a validated `Resolved*` with
//! every default applied and every semantic check made exactly once. The
//! dispatcher in [`crate::parse::parse_request`] composes the two.

pub(crate) mod get_tile;

pub use get_tile::ResolvedGetTile;

pub(crate) use get_tile::resolve_get_tile;

use mars_types::ImageFormat;

/// Option-heavy GetTile shape produced by either the KVP or REST parser in
/// [`crate::parse::get_tile`]. Both transports converge here, so the
/// resolver in [`get_tile`] is the only place bbox math, TMS lookup, and
/// allowlist checks live - REST and KVP cache keys can never drift.
///
/// `style` is intentionally not modelled today: the renderer does not yet
/// route per-style. Add it back once there is a consumer rather than
/// carrying a field nothing reads.
#[derive(Debug, Default, Clone)]
pub(crate) struct ParsedGetTile {
    pub version: Option<String>,
    pub layer: Option<String>,
    /// Format, already lowered from MIME (KVP) or extension (REST) to the
    /// canonical enum. Allowlist enforcement happens in prepare.
    pub format: Option<ImageFormat>,
    pub tilematrixset: Option<String>,
    pub tilematrix: Option<String>,
    pub tilecol: Option<u32>,
    pub tilerow: Option<u32>,
}

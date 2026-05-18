//! Errors produced by source adapters.

/// Errors produced by source adapters.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// The adapter does not implement this method yet. Used by Phase-0 stubs.
    #[error("not implemented: {what}")]
    NotImplemented {
        /// Human-readable name of the unimplemented operation.
        what: &'static str,
    },
    /// Connectivity, transport, or driver error. `what` is a stable short
    /// label callers can match on; `source` carries the original adapter
    /// error chain so `anyhow`'s `{:#}` walks the backend error code / cause
    /// without forcing a port-level dependency on a specific driver.
    #[error("backend: {what}")]
    Backend {
        /// Stable short label for what was being attempted.
        what: &'static str,
        /// Original error chain.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
    /// The change feed cursor was lost or fell too far behind. Recovery is via
    /// snapshot compile.
    #[error("change feed gone; full snapshot required")]
    ChangeFeedGone,
    /// Invalid binding configuration.
    #[error("invalid binding: {0}")]
    InvalidBinding(String),
    /// The upstream confirmed no tile exists at this position (e.g. HTTP 404 /
    /// 204 from an XYZ pyramid). Distinct from `Backend` so callers can treat
    /// absence as a normal sparse-coverage signal rather than a hard failure.
    #[error("tile absent at z={z} x={x} y={y}")]
    TileAbsent {
        /// Zoom level.
        z: u32,
        /// Tile column index.
        x: u32,
        /// Tile row index.
        y: u32,
    },
    /// A filter expression referenced an identifier outside the binding's
    /// allowlist (`binding.attributes ∪ {binding.id_field}`). The adapter's
    /// filter lowering refuses to inject unknown identifiers.
    #[error("unknown identifier: {name}")]
    UnknownIdent {
        /// Identifier that was not present in the allowlist.
        name: String,
    },
}

/// String-only error usable as a `Backend.source` chain when the originating
/// site has no real `Error` to wrap (invariant violations, missing config
/// fields). Kept private; callers go through [`SourceError::backend_msg`].
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct BackendMessage(String);

impl SourceError {
    /// Build a `Backend` error wrapping an existing error chain. `what` is a
    /// stable short label for the operation; `source` carries the original
    /// driver / adapter error so the chain survives.
    pub fn backend(what: &'static str, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Backend {
            what,
            source: Box::new(source),
        }
    }

    /// Build a `Backend` error from a static label and a free-form message
    /// for sites that have no inner error to wrap (invariant violations etc.).
    pub fn backend_msg(what: &'static str, msg: impl Into<String>) -> Self {
        Self::Backend {
            what,
            source: Box::new(BackendMessage(msg.into())),
        }
    }
}

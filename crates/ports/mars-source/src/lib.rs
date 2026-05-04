//! Port traits for source databases and change feeds.
//!
//! `Source` is the read interface used by the compiler to materialise
//! geometries and attributes for a `(source_collection, scale_band, cell)`.
//! `ChangeFeed` is the subscription interface that produces dirty-cell
//! events (SPEC §8.2). Both are runtime-agnostic — concrete adapters live
//! in `crates/adapters/mars-source-*`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use mars_expr::Expr;
use mars_types::{Bbox, Cell};

/// Errors produced by source adapters.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// The adapter does not implement this method yet. Used by Phase-0 stubs.
    #[error("not implemented: {what}")]
    NotImplemented {
        /// Human-readable name of the unimplemented operation.
        what: &'static str,
    },
    /// Connectivity, transport, or driver error.
    #[error("backend error: {0}")]
    Backend(String),
    /// The change feed slot was lost or fell too far behind. Recovery is via
    /// snapshot compile (SPEC §8.2.3).
    #[error("change feed gone; full snapshot required")]
    ChangeFeedGone,
}

/// One change-feed event, lowered from a postgres logical-decoding message
/// or a polling diff.
#[derive(Debug, Clone)]
pub enum ChangeEvent {
    /// A row was inserted; new geometry occupies the listed cells.
    Insert {
        /// Logical name of the source table / collection.
        collection: String,
        /// Cells touched by the new geometry.
        cells: Vec<Cell>,
    },
    /// A row was updated; cells = touched-by-old ∪ touched-by-new.
    Update {
        /// Logical name of the source table / collection.
        collection: String,
        /// Cells touched by the old or new geometry.
        cells: Vec<Cell>,
    },
    /// A row was deleted; cells = touched-by-old.
    Delete {
        /// Logical name of the source table / collection.
        collection: String,
        /// Cells touched by the old geometry.
        cells: Vec<Cell>,
    },
    /// The whole collection was truncated; every cell is dirty.
    Truncate {
        /// Logical name of the source table / collection.
        collection: String,
    },
}

/// Read-side port: query geometry and attributes for a cell.
#[async_trait]
pub trait Source: Send + Sync + 'static {
    /// Materialise the rows for `collection` whose geometry intersects
    /// `bbox`, optionally filtered by a `mars-expr` AST.
    async fn fetch_cell(
        &self,
        collection: &str,
        bbox: Bbox,
        filter: Option<&Expr>,
    ) -> Result<Vec<RowBytes>, SourceError>;
}

/// Opaque per-row bytes returned by `Source`. The exact shape (WKB vs
/// adapter-native) is up to the adapter; the compiler treats it as opaque
/// and re-encodes into the artifact format.
#[derive(Debug, Clone)]
pub struct RowBytes {
    /// Stable identifier of the row inside the source collection.
    pub feature_id: u64,
    /// Encoded geometry payload.
    pub geometry: Bytes,
    /// Encoded attribute payload.
    pub attributes: Bytes,
}

/// Subscription-side port: a stream of `ChangeEvent`s.
#[async_trait]
pub trait ChangeFeed: Send + Sync + 'static {
    /// Subscribe to the change feed. The returned stream lives for the
    /// lifetime of the compiler process; transient errors are reported
    /// inline as `Result::Err`.
    async fn subscribe(&self) -> Result<BoxStream<'static, Result<ChangeEvent, SourceError>>, SourceError>;
}

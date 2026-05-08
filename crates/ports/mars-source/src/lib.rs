//! Port traits for source databases and change feeds.
//!
//! `Source` is the read interface used by the compiler to materialise
//! geometries and attributes per page (page-keyed, LAZARUS Phase C+).
//! `ChangeFeed` is the subscription interface that produces dirty-page
//! events (SPEC §8.2). Both are runtime-agnostic - concrete adapters live
//! in `crates/adapters/mars-source-*`.
//!
//! Phase B note: the cell-keyed surface is retired with the v3 substrate
//! cut; Phase C reintroduces page-keyed `fetch_full_table_streaming` and
//! `fetch_by_feature_ids` plus a `ChangeEvent` payload that carries the
//! geometry envelope so the compiler can derive Hilbert keys directly.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod access;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use mars_types::CrsCode;
pub use mars_types::SourceCollectionId;

pub use access::RowAttrs;

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
    /// error chain so `anyhow`'s `{:#}` walks SQLSTATE / severity / cause
    /// without forcing a port-level dependency on a specific driver.
    #[error("backend: {what}")]
    Backend {
        /// Stable short label for what was being attempted.
        what: &'static str,
        /// Original error chain.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
    /// The change feed slot was lost or fell too far behind. Recovery is via
    /// snapshot compile (SPEC §8.2.3).
    #[error("change feed gone; full snapshot required")]
    ChangeFeedGone,
    /// Invalid binding configuration.
    #[error("invalid binding: {0}")]
    InvalidBinding(String),
    /// A filter expression referenced an identifier outside the binding's
    /// allowlist (`binding.attributes ∪ {binding.id_column}`). SQL lowering
    /// refuses to inject unknown identifiers.
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

/// One change-feed event, lowered from a postgres logical-decoding message
/// or a polling diff.
///
/// Phase B carries only the collection identity; Phase C adds the per-event
/// geometry envelope (raw WKB + bbox/centroid for old/new rows under
/// `REPLICA IDENTITY FULL`) so the compiler can derive each row's Hilbert
/// key without round-tripping through a cell grid.
#[derive(Debug, Clone)]
pub enum ChangeEvent {
    /// A row was inserted.
    Insert {
        /// Logical name of the source table / collection.
        collection: SourceCollectionId,
    },
    /// A row was updated.
    Update {
        /// Logical name of the source table / collection.
        collection: SourceCollectionId,
    },
    /// A row was deleted.
    Delete {
        /// Logical name of the source table / collection.
        collection: SourceCollectionId,
    },
    /// The whole collection was truncated; the binding goes through a
    /// bootstrap-equivalent rebuild.
    Truncate {
        /// Logical name of the source table / collection.
        collection: SourceCollectionId,
    },
}

/// A committed batch of change events. Adapters MUST only emit a batch once
/// the upstream transaction has committed; the `source_version` cursor (e.g.
/// pgoutput LSN) lets the compiler advance manifest provenance atomically
/// per batch.
#[derive(Debug, Clone)]
pub struct ChangeBatch {
    /// Ordered events committed together.
    pub events: Vec<ChangeEvent>,
    /// Opaque source-side cursor identifying the committed position. `None`
    /// when the adapter has no notion of a cursor (polling fallback).
    pub source_version: Option<String>,
}

/// Source-side binding: maps a logical `SourceCollectionId` onto the physical
/// table, geometry/id columns, attribute projection, and CRS. Lives in
/// `mars-source` because every field is database-vocabulary, not domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceBinding {
    /// Logical collection name.
    pub collection: SourceCollectionId,
    /// Source schema (e.g. `public`).
    pub from_schema: String,
    /// Source table.
    pub from_table: String,
    /// Geometry column name.
    pub geometry_column: String,
    /// Stable feature-id column name.
    pub id_column: String,
    /// Ordered, deduplicated attribute projection.
    pub attributes: Vec<String>,
    /// Source CRS.
    pub crs: CrsCode,
}

impl SourceBinding {
    /// Construct after validating attribute uniqueness and non-empty
    /// schema/table/column names.
    pub fn new(
        collection: SourceCollectionId,
        from_schema: impl Into<String>,
        from_table: impl Into<String>,
        geometry_column: impl Into<String>,
        id_column: impl Into<String>,
        attributes: Vec<String>,
        crs: CrsCode,
    ) -> Result<Self, SourceError> {
        let from_schema = from_schema.into();
        let from_table = from_table.into();
        let geometry_column = geometry_column.into();
        let id_column = id_column.into();

        for (label, v) in [
            ("from_schema", &from_schema),
            ("from_table", &from_table),
            ("geometry_column", &geometry_column),
            ("id_column", &id_column),
        ] {
            if v.is_empty() {
                return Err(SourceError::InvalidBinding(format!("{label} is empty")));
            }
        }

        // dedup check, preserves order
        let mut seen = std::collections::HashSet::with_capacity(attributes.len());
        for a in &attributes {
            if a.is_empty() {
                return Err(SourceError::InvalidBinding("empty attribute name".into()));
            }
            if !seen.insert(a.as_str()) {
                return Err(SourceError::InvalidBinding(format!("duplicate attribute: {a}")));
            }
        }

        Ok(Self {
            collection,
            from_schema,
            from_table,
            geometry_column,
            id_column,
            attributes,
            crs,
        })
    }
}

/// Decoded attribute value. Mirrors `mars_expr::Literal` 1:1, lives in
/// `mars-source` because the source layer is where rows are materialised
/// before evaluator handoff.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrValue {
    /// SQL NULL.
    Null,
    /// Boolean.
    Bool(bool),
    /// 64-bit signed integer.
    Int(i64),
    /// 64-bit float.
    Float(f64),
    /// UTF-8 string.
    String(String),
}

/// Read-side port. Phase C surface (LAZARUS):
/// - `fetch_full_table_streaming(binding)` for snapshot bootstrap, and
/// - `fetch_by_feature_ids(binding, ids)` for incremental page rebuilds
///   (PK-indexed, `WHERE id = ANY($1)`) — added in Phase C.2.
#[async_trait]
pub trait Source: Send + Sync + 'static {
    /// Stream every row of `binding`'s table in undefined order. Cursor /
    /// pipelined under the hood so the compiler can sort externally without
    /// pulling the full result set into RAM. Adapters that have not yet
    /// implemented this surface return `SourceError::NotImplemented`.
    async fn fetch_full_table_streaming<'a>(
        &'a self,
        binding: &'a SourceBinding,
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError>;
}

/// Per-row record returned by `Source`. Geometry is opaque adapter-native
/// bytes (typically WKB); attributes are decoded into `(name, AttrValue)`
/// pairs ordered to match the binding's projection.
#[derive(Debug, Clone)]
pub struct RowBytes {
    /// Stable identifier of the row inside the source collection.
    pub feature_id: u64,
    /// Encoded geometry payload (opaque to the compiler).
    pub geometry: Bytes,
    /// Decoded attributes, ordered to mirror `SourceBinding.attributes`.
    pub attributes: Vec<(String, AttrValue)>,
}

/// Phase-0 stub adapters that satisfy the port traits with `NotImplemented`.
/// Lets bins and tests compose the surface without naming a real backend.
pub mod stub {
    use super::{BoxStream, ChangeFeed, ChangeSubscription, RowBytes, Source, SourceBinding, SourceError};
    use async_trait::async_trait;

    /// `Source` + `ChangeFeed` impl that always returns `NotImplemented`.
    #[derive(Debug, Default)]
    pub struct NotImplementedSource;

    #[async_trait]
    impl Source for NotImplementedSource {
        async fn fetch_full_table_streaming<'a>(
            &'a self,
            _binding: &'a SourceBinding,
        ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
            Err(SourceError::NotImplemented {
                what: "fetch_full_table_streaming",
            })
        }
    }

    #[async_trait]
    impl ChangeFeed for NotImplementedSource {
        async fn subscribe(&self) -> Result<Box<dyn ChangeSubscription>, SourceError> {
            Err(SourceError::NotImplemented {
                what: "mars-source::stub::NotImplementedSource::subscribe",
            })
        }
    }
}

/// Subscription-side port: a stream of committed [`ChangeBatch`]es with an
/// ack-aware cursor.
///
/// `subscribe` opens the subscription. The returned [`ChangeSubscription`] is
/// drained by the compiler one batch at a time; the compiler MUST call
/// [`ChangeSubscription::acknowledge`] only after a batch's effects have been
/// durably committed to the manifest store. Non-acked batches must be
/// re-delivered after a restart.
#[async_trait]
pub trait ChangeFeed: Send + Sync + 'static {
    /// Open a fresh subscription. Each call returns an independent cursor
    /// positioned at the last acknowledged source version (or earliest
    /// available, on first connect).
    async fn subscribe(&self) -> Result<Box<dyn ChangeSubscription>, SourceError>;
}

/// Owned, ack-aware subscription. Pulled by the compiler one batch at a time.
///
/// `acknowledge` MUST only be called once a batch's effects are durably
/// recorded in the manifest store, so a crash between `next_batch` and
/// `acknowledge` re-delivers the batch on reconnect.
#[async_trait]
pub trait ChangeSubscription: Send {
    /// Pull the next committed batch. `None` indicates the feed closed
    /// cleanly; transient errors surface as `Some(Err(_))` and the caller
    /// decides whether to abort.
    async fn next_batch(&mut self) -> Option<Result<ChangeBatch, SourceError>>;

    /// Acknowledge that every batch up to and including `source_version` is
    /// durably persisted downstream. Adapters with no notion of a cursor
    /// (polling fallback) treat this as a no-op.
    async fn acknowledge(&mut self, source_version: Option<&str>) -> Result<(), SourceError>;

    /// Gracefully tear down the subscription, awaiting any background worker
    /// so that a final feedback ack is on the wire before the call returns.
    /// Callers must invoke this on every shutdown path; `Drop` is a fallback
    /// that may abort in-flight work without waiting. Default impl is a no-op
    /// for adapters with no detached workers.
    async fn shutdown(&mut self) -> Result<(), SourceError> {
        Ok(())
    }
}

/// Opaque guard returned by [`LeaderLock::try_acquire`]. Holding it keeps the
/// underlying lock; dropping it releases the lock (typically by closing the
/// session that owns it).
pub trait LeaderLockGuard: Send + Sync + std::fmt::Debug {}

/// Coordination port: at most one compiler instance per service holds the
/// leader lock at a time. The hash is precomputed by the application so the
/// adapter does not need to know about service identity.
#[async_trait]
pub trait LeaderLock: Send + Sync + 'static {
    /// Try to acquire the lock keyed by `key`. Non-blocking:
    /// - `Ok(Some(guard))` — leader; hold `guard` for the duration of work.
    /// - `Ok(None)` — another instance holds the lock.
    /// - `Err(_)` — backend error.
    async fn try_acquire(&self, key: i64) -> Result<Option<Box<dyn LeaderLockGuard>>, SourceError>;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn binding_constructor_accepts_valid() {
        let b = SourceBinding::new(
            SourceCollectionId::new("roads"),
            "public",
            "roads",
            "geom",
            "gid",
            vec!["name".into(), "class".into()],
            CrsCode::new("EPSG:25832"),
        )
        .unwrap();
        assert_eq!(b.collection.as_str(), "roads");
        assert_eq!(b.from_schema, "public");
        assert_eq!(b.from_table, "roads");
        assert_eq!(b.geometry_column, "geom");
        assert_eq!(b.id_column, "gid");
        assert_eq!(b.attributes, vec!["name".to_string(), "class".to_string()]);
        assert_eq!(b.crs.as_str(), "EPSG:25832");
    }

    #[test]
    fn binding_rejects_duplicate_attribute() {
        let r = SourceBinding::new(
            SourceCollectionId::new("c"),
            "s",
            "t",
            "g",
            "id",
            vec!["a".into(), "a".into()],
            CrsCode::new("EPSG:4326"),
        );
        assert!(matches!(r, Err(SourceError::InvalidBinding(_))));
    }

    #[test]
    fn binding_rejects_empty_field() {
        let r = SourceBinding::new(
            SourceCollectionId::new("c"),
            "",
            "t",
            "g",
            "id",
            vec![],
            CrsCode::new("EPSG:4326"),
        );
        assert!(matches!(r, Err(SourceError::InvalidBinding(_))));
    }

    // phase-c will reintroduce page-keyed Source surface and its tests.
}

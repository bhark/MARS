//! Port traits for source databases and change feeds.
//!
//! `Source` is the read interface used by the compiler to materialise
//! geometries and attributes per page. `ChangeFeed` is the subscription
//! interface that produces dirty-page events. Both are
//! runtime-agnostic - concrete adapters live in `crates/adapters/mars-source-*`.
//!
//! `CompileSession` exposes a snapshot-stable `fetch_full_table_streaming`
//! that reuses the pass-1 transaction.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod access;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
pub use mars_types::SourceCollectionId;
use mars_types::{Bbox, CrsCode};

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
    /// snapshot compile.
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

/// geometry summary carried by a change event.
#[derive(Debug, Clone, PartialEq)]
pub struct GeometryEnvelope {
    /// centroid in canonical crs.
    pub centroid: [f64; 2],
    /// axis-aligned row bounds.
    pub bbox: Bbox,
}

/// One change-feed event, lowered from a postgres logical-decoding message
/// or a polling diff.
///
/// `old_envelope` is `Some` only when the feed supplies the old row, e.g.
/// postgres `REPLICA IDENTITY FULL`.
#[derive(Debug, Clone)]
pub enum ChangeEvent {
    /// A row was inserted.
    Insert {
        /// Logical name of the source table / collection.
        collection: SourceCollectionId,
        /// stable feature id.
        feature_id: u64,
        /// inserted-row envelope.
        new_envelope: GeometryEnvelope,
    },
    /// A row was updated.
    Update {
        /// Logical name of the source table / collection.
        collection: SourceCollectionId,
        /// stable feature id.
        feature_id: u64,
        /// new-row envelope.
        new_envelope: GeometryEnvelope,
        /// old-row envelope, if supplied by the feed.
        old_envelope: Option<GeometryEnvelope>,
    },
    /// A row was deleted.
    Delete {
        /// Logical name of the source table / collection.
        collection: SourceCollectionId,
        /// stable feature id.
        feature_id: u64,
        /// deleted-row envelope, if supplied by the feed.
        old_envelope: Option<GeometryEnvelope>,
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
#[derive(Debug, Clone, PartialEq)]
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
    /// Optional binding-level filter ANDed into every SELECT this binding
    /// drives. Idents must already be in `attributes ∪ {id_column}` (the
    /// caller is expected to validate up front; lowering double-checks).
    pub filter: Option<mars_expr::Expr>,
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
            filter: None,
        })
    }

    /// Attach (or clear) a binding-level filter expression. Lowering ANDs
    /// it into the source SELECT in addition to any caller-supplied filter.
    #[must_use]
    pub fn with_filter(mut self, filter: Option<mars_expr::Expr>) -> Self {
        self.filter = filter;
        self
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

/// Read-side port.
/// - `fetch_full_table_streaming(binding)` for snapshot bootstrap, and
/// - `fetch_by_feature_ids(binding, ids)` for incremental page rebuilds
///   (`WHERE id_column = ANY($1)`; bag-valued — sources are allowed to
///   return multiple rows per id, in which case the compiler treats each
///   row as a distinct substrate feature for rendering purposes).
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

    /// stream rows matching feature ids.
    async fn fetch_by_feature_ids<'a>(
        &'a self,
        binding: &'a SourceBinding,
        ids: &'a [i64],
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError>;

    /// Stream every feature id present in the binding's table, in undefined
    /// order. Used by the periodic reconciliation hook to compare the source
    /// id set against the page-membership sidecar's id set without paying the
    /// cost of a full geometry/attribute fetch.
    async fn stream_feature_ids<'a>(
        &'a self,
        binding: &'a SourceBinding,
    ) -> Result<BoxStream<'a, Result<i64, SourceError>>, SourceError>;

    /// Open a compile-time session against `binding`. The returned session
    /// holds one connection in a snapshot-isolated transaction so a pass-1
    /// geometry summary scan and a pass-2 row hydration scan see identical
    /// row sets. Non-snapshot callers (incremental cycles) keep using the
    /// stateless `fetch_*` methods above. Default impl is `NotImplemented`
    /// so adapters opt in explicitly.
    async fn open_compile_session<'a>(
        &'a self,
        _binding: &'a SourceBinding,
    ) -> Result<Box<dyn CompileSession + 'a>, SourceError> {
        Err(SourceError::NotImplemented {
            what: "open_compile_session",
        })
    }
}

/// Opaque per-row identity stable within a single `CompileSession`'s
/// snapshot. Adapters populate per their backing store; the postgres
/// adapter packs `tableoid` then `BlockNumber` then `OffsetNumber` (10
/// useful bytes, zero-padded). 16 bytes leaves room for future
/// row-routing metadata without a port-wide widening.
///
/// The page planner uses it as a strict tiebreaker after
/// `(hilbert_key, feature_id)` to make pass-1 sort fully deterministic
/// without a server-side digest pass over the geometry bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceRowKey([u8; 16]);

impl SourceRowKey {
    /// Build a key from its 16-byte representation.
    #[must_use]
    pub const fn from_bytes(b: [u8; 16]) -> Self {
        Self(b)
    }

    /// Borrow the raw 16-byte representation.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Zero key. Useful as a placeholder in fakes / tests where adapter
    /// row identity is not modeled.
    pub const ZERO: Self = Self([0u8; 16]);
}

/// Per-row geometry summary produced by [`CompileSession::fetch_geometry_summary`].
/// Bbox + byte length + a snapshot-stable row identity — exactly the
/// fixed-size record the pass-1 page planner needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RowSummary {
    /// Stable identifier of the row inside the source collection.
    pub feature_id: i64,
    /// Axis-aligned bounds in native CRS units, [xmin, ymin, xmax, ymax].
    pub bbox: [f32; 4],
    /// Length of the encoded geometry in bytes; pass 1 uses this as the
    /// per-row contribution to the page-byte sweep.
    pub geom_byte_length: u32,
    /// Snapshot-stable row identity. Used as the page planner's terminal
    /// sort tier after `(hilbert_key, feature_id)`.
    pub row_key: SourceRowKey,
}

/// Compile-time session bound to one `SourceBinding`. Holds a connection in
/// a snapshot-isolated transaction so pass-1 (geometry summary) and pass-2
/// (full-table row hydration) see the same rows.
///
/// `&mut self` on the streaming methods enforces one stream at a time:
/// callers drain (or drop) one stream before opening the next.
///
/// `RowSummary::row_key` carries an opaque snapshot-stable row identity
/// (Postgres: `tableoid + ctid`) used by the page planner as the terminal
/// sort tier after `(hilbert_key, feature_id)`.
#[async_trait]
pub trait CompileSession: Send + Sync {
    /// Stream a per-row geometry summary across the bound table. Pass 1
    /// of the unified compile pipeline.
    async fn fetch_geometry_summary<'a>(
        &'a mut self,
    ) -> Result<BoxStream<'a, Result<RowSummary, SourceError>>, SourceError>;

    /// Stream every row of the bound table from the same snapshot, in
    /// adapter-native order. Pass 2 of the unified compile pipeline.
    ///
    /// Each yielded `RowBytes` carries a snapshot-stable `row_key` derived
    /// from the same source as `RowSummary::row_key` (postgres: `tableoid +
    /// ctid`); the compiler buckets rows into the planned pages by joining
    /// on `row_key`. Bag-valued: rows that exploded into multiple parts
    /// upstream are still returned as distinct rows.
    async fn fetch_full_table_streaming<'a>(
        &'a mut self,
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError>;

    /// Commit the snapshot transaction. Call on the success path of a
    /// compile session.
    async fn commit(self: Box<Self>) -> Result<(), SourceError>;

    /// Roll back the snapshot transaction. Call on the error path of a
    /// compile session. `Drop` does not perform I/O — adapters rely on
    /// pool-level recycling for safety, so callers should still invoke
    /// `commit` or `rollback` explicitly.
    async fn rollback(self: Box<Self>) -> Result<(), SourceError>;
}

/// Per-row record returned by `Source` and `CompileSession`. Geometry is
/// opaque adapter-native bytes (typically WKB); attributes are decoded into
/// `(name, AttrValue)` pairs ordered to match the binding's projection.
///
/// `row_key` carries the snapshot-stable row identity when the row was
/// produced inside a `CompileSession`. Stateless `Source::fetch_*` callers
/// have no transactional snapshot to hang an identity off and set it to
/// [`SourceRowKey::ZERO`].
#[derive(Debug, Clone)]
pub struct RowBytes {
    /// Stable identifier of the row inside the source collection.
    pub feature_id: u64,
    /// Encoded geometry payload (opaque to the compiler).
    pub geometry: Bytes,
    /// Decoded attributes, ordered to mirror `SourceBinding.attributes`.
    pub attributes: Vec<(String, AttrValue)>,
    /// Snapshot-stable row identity. Populated by `CompileSession`; set to
    /// [`SourceRowKey::ZERO`] by stateless `Source` callers.
    pub row_key: SourceRowKey,
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

        async fn fetch_by_feature_ids<'a>(
            &'a self,
            _binding: &'a SourceBinding,
            _ids: &'a [i64],
        ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
            Err(SourceError::NotImplemented {
                what: "fetch_by_feature_ids",
            })
        }

        async fn stream_feature_ids<'a>(
            &'a self,
            _binding: &'a SourceBinding,
        ) -> Result<BoxStream<'a, Result<i64, SourceError>>, SourceError> {
            Err(SourceError::NotImplemented {
                what: "stream_feature_ids",
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

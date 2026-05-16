//! Port traits for source backends and change feeds.
//!
//! `Source` is the read interface used by the compiler to materialise
//! geometries and attributes per page. `ChangeFeed` is the subscription
//! interface that produces dirty-page events. Both are
//! runtime-agnostic - concrete adapters live in `crates/adapters/mars-source-*`.
//!
//! `CompileSession` exposes a snapshot-stable `stream_rows` that reuses the
//! pass-1 transaction.

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

/// geometry summary carried by a change event.
#[derive(Debug, Clone, PartialEq)]
pub struct GeometryEnvelope {
    /// centroid in canonical crs.
    pub centroid: [f64; 2],
    /// axis-aligned row bounds.
    pub bbox: Bbox,
}

/// One change-feed event, lowered from an upstream change-feed message or a
/// polling diff. Update / Delete carry only the new-side envelope (or none,
/// for Delete) - old-side dirty pages are resolved downstream through the
/// page-membership sidecar keyed by `feature_id`.
#[derive(Debug, Clone)]
pub enum ChangeEvent {
    /// A row was inserted.
    Insert {
        /// Logical name of the source collection.
        collection: SourceCollectionId,
        /// stable feature id.
        feature_id: u64,
        /// inserted-row envelope.
        new_envelope: GeometryEnvelope,
    },
    /// A row was updated.
    Update {
        /// Logical name of the source collection.
        collection: SourceCollectionId,
        /// stable feature id.
        feature_id: u64,
        /// new-row envelope.
        new_envelope: GeometryEnvelope,
    },
    /// A row was deleted.
    Delete {
        /// Logical name of the source collection.
        collection: SourceCollectionId,
        /// stable feature id.
        feature_id: u64,
    },
    /// The whole collection was truncated; the binding goes through a
    /// bootstrap-equivalent rebuild.
    Truncate {
        /// Logical name of the source collection.
        collection: SourceCollectionId,
    },
    /// The underlying object backing the binding was replaced (e.g. a
    /// swap-and-rename pipeline rebuilt the table, a partition was
    /// rotated, a publication membership change was detected). The
    /// adapter signals this distinctly from `Truncate` so operators can
    /// tell the two causes apart in traces and metrics. The compiler
    /// reacts based on `reason`:
    /// - `OidChanged` triggers a per-binding resnapshot equivalent to
    ///   `Truncate`.
    /// - `PreflightFailed` or `BindingUnpublished` mark the binding
    ///   degraded; the prior manifest pages stay served and the cycle
    ///   skips the rebuild via the per-binding failure-isolation policy.
    Rebind {
        /// Logical name of the source collection.
        collection: SourceCollectionId,
        /// Why the rebind was raised.
        reason: RebindReason,
    },
}

/// Reason a [`ChangeEvent::Rebind`] was emitted. Carried so the compiler
/// can dispatch (resnapshot vs degrade) and so operators see the cause
/// distinctly in traces and metric labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebindReason {
    /// The change feed delivered a backend identity transition for a
    /// bound name (e.g. pgoutput Relation message with a new relation
    /// OID for a known schema/table). Preflight passed; the cache was
    /// rebound. The compiler resnapshots the binding to repair the
    /// manifest slice for the previous identity.
    OidChanged {
        /// Previous backend identity that the cache held.
        old_oid: u32,
        /// New backend identity to route under going forward.
        new_oid: u32,
    },
    /// Backend identity transition arrived but the new object failed
    /// preflight (e.g. REPLICA IDENTITY != FULL, geometry/id column
    /// missing). The cache rejected the rebind; the binding is degraded
    /// until the operator fixes the source.
    PreflightFailed {
        /// Human-readable failure reason for the operator-facing log.
        reason: String,
    },
    /// A periodic catalog probe found the bound name absent from the
    /// publication / catalog. The change feed cannot deliver a Relation
    /// for it, so the adapter surfaces the gap directly. Treated like
    /// `PreflightFailed`: degrade, do not silently drop pages.
    BindingUnpublished,
}

/// Health classification for a single binding, returned by
/// [`Source::probe_binding_health`]. Adapters with no notion of
/// publication membership rely on the default impl which always reports
/// `Healthy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingHealth {
    /// The binding's backing object exists and is wired for change
    /// delivery (e.g. present in the postgres publication).
    Healthy(SourceCollectionId),
    /// The binding's backing object is not currently published. The
    /// change feed will not deliver events for it; the compiler should
    /// degrade the binding via the per-binding failure-isolation path.
    Unpublished(SourceCollectionId),
}

/// A committed batch of change events. Adapters MUST only emit a batch once
/// the upstream transaction has committed; the `source_version` cursor lets
/// the compiler advance manifest provenance atomically per batch.
#[derive(Debug, Clone)]
pub struct ChangeBatch {
    /// Ordered events committed together.
    pub events: Vec<ChangeEvent>,
    /// Opaque backend-side cursor identifying the committed position (e.g.
    /// WAL position, change-stream token, ETag). `None` when the adapter has
    /// no notion of a cursor (polling fallback).
    pub source_version: Option<String>,
}

/// Source-side binding: maps a logical `SourceCollectionId` onto a backend
/// locator, geometry/id fields, attribute projection, and CRS. Lives in
/// `mars-source` because every field is a backend-shaped locator, not domain.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceBinding {
    /// Logical collection name.
    pub collection: SourceCollectionId,
    /// Opaque backend-side locator for the source records. Format is defined
    /// by the adapter (e.g. `"schema.table"` for relational backends, a
    /// collection path for document stores, an object-store prefix, etc.).
    pub from: String,
    /// Geometry field name within each record.
    pub geometry_field: String,
    /// Stable feature-id field name within each record.
    pub id_field: String,
    /// Ordered, deduplicated attribute projection.
    pub attributes: Vec<String>,
    /// Source CRS.
    pub crs: CrsCode,
    /// Optional binding-level filter applied to every scan this binding
    /// drives. Idents must already be in `attributes ∪ {id_field}` (the
    /// caller is expected to validate up front; lowering double-checks).
    pub filter: Option<mars_expr::Expr>,
    /// Optional adapter-side DSN / connection-string override. Currently
    /// honoured only by the postgis adapter, which routes the binding to a
    /// per-DSN connection pool. Vector-file and other adapters ignore this
    /// field. Carried on the port binding (rather than out-of-band) so the
    /// adapter sees the override at the same boundary it sees the locator.
    pub dsn: Option<String>,
}

impl SourceBinding {
    /// Construct after validating attribute uniqueness and non-empty
    /// locator / field names.
    pub fn new(
        collection: SourceCollectionId,
        from: impl Into<String>,
        geometry_field: impl Into<String>,
        id_field: impl Into<String>,
        attributes: Vec<String>,
        crs: CrsCode,
    ) -> Result<Self, SourceError> {
        let from = from.into();
        let geometry_field = geometry_field.into();
        let id_field = id_field.into();

        for (label, v) in [
            ("from", &from),
            ("geometry_field", &geometry_field),
            ("id_field", &id_field),
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
            from,
            geometry_field,
            id_field,
            attributes,
            crs,
            filter: None,
            dsn: None,
        })
    }

    /// Attach (or clear) a binding-level filter expression. Lowering applies
    /// it to the source scan in addition to any caller-supplied filter.
    #[must_use]
    pub fn with_filter(mut self, filter: Option<mars_expr::Expr>) -> Self {
        self.filter = filter;
        self
    }

    /// Attach (or clear) a binding-level DSN override. Honoured by adapters
    /// that route per-DSN (postgis); ignored elsewhere.
    #[must_use]
    pub fn with_dsn(mut self, dsn: Option<String>) -> Self {
        self.dsn = dsn;
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
/// - `stream_rows(binding)` for snapshot bootstrap, and
/// - `stream_rows_by_id(binding, ids)` for incremental page rebuilds
///   (filtered to the supplied id set; bag-valued - sources are allowed to
///   return multiple rows per id, in which case the compiler treats each
///   row as a distinct substrate feature for rendering purposes).
#[async_trait]
pub trait Source: Send + Sync + 'static {
    /// Stream every row of `binding`'s collection in undefined order.
    /// Streamed under the hood so the compiler can sort externally without
    /// pulling the full result set into RAM. Adapters that have not yet
    /// implemented this surface return `SourceError::NotImplemented`.
    async fn stream_rows<'a>(
        &'a self,
        binding: &'a SourceBinding,
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError>;

    /// stream rows matching feature ids.
    async fn stream_rows_by_id<'a>(
        &'a self,
        binding: &'a SourceBinding,
        ids: &'a [i64],
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError>;

    /// Stream every feature id present in the binding's collection, in
    /// undefined order. Used by the periodic reconciliation hook to compare
    /// the source id set against the page-membership sidecar's id set
    /// without paying the cost of a full geometry/attribute fetch.
    async fn stream_feature_ids<'a>(
        &'a self,
        binding: &'a SourceBinding,
    ) -> Result<BoxStream<'a, Result<i64, SourceError>>, SourceError>;

    /// Open a compile-time session against `binding`. The returned session
    /// holds one connection in a snapshot-isolated transaction so a pass-1
    /// geometry summary scan and a pass-2 row hydration scan see identical
    /// row sets. Non-snapshot callers (incremental cycles) keep using the
    /// stateless `stream_*` methods above. Default impl is `NotImplemented`
    /// so adapters opt in explicitly.
    async fn open_compile_session<'a>(
        &'a self,
        _binding: &'a SourceBinding,
    ) -> Result<Box<dyn CompileSession + 'a>, SourceError> {
        Err(SourceError::NotImplemented {
            what: "open_compile_session",
        })
    }

    /// Periodic backstop for bindings whose backing object disappears
    /// without an in-band change-feed signal (e.g. a publication-membership
    /// drop with no replacement). Adapters that have no analogous concept
    /// fall through to the default impl, which reports every requested
    /// binding `Healthy`.
    async fn probe_binding_health(
        &self,
        collections: &[SourceCollectionId],
    ) -> Result<Vec<BindingHealth>, SourceError> {
        Ok(collections.iter().cloned().map(BindingHealth::Healthy).collect())
    }
}

/// Opaque per-row identity stable within a single `CompileSession`'s
/// snapshot. Adapters populate per their backing store. 16 bytes leaves
/// room for future row-routing metadata without a port-wide widening.
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

/// Per-row geometry summary produced by [`CompileSession::stream_geometry_summary`].
/// Bbox + byte length + a snapshot-stable row identity - exactly the
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
/// (full row hydration) see the same rows.
///
/// `&mut self` on the streaming methods enforces one stream at a time:
/// callers drain (or drop) one stream before opening the next.
///
/// `RowSummary::row_key` carries an opaque snapshot-stable row identity
/// used by the page planner as the terminal sort tier after
/// `(hilbert_key, feature_id)`.
#[async_trait]
pub trait CompileSession: Send + Sync {
    /// Stream a per-row geometry summary across the bound collection. Pass 1
    /// of the unified compile pipeline.
    async fn stream_geometry_summary<'a>(
        &'a mut self,
    ) -> Result<BoxStream<'a, Result<RowSummary, SourceError>>, SourceError>;

    /// Stream every row of the bound collection from the same snapshot, in
    /// adapter-native order. Pass 2 of the unified compile pipeline.
    ///
    /// Each yielded `RowBytes` carries a snapshot-stable `row_key` derived
    /// from the same source as `RowSummary::row_key`; the compiler buckets
    /// rows into the planned pages by joining on `row_key`. Bag-valued: rows
    /// that exploded into multiple parts upstream are still returned as
    /// distinct rows.
    async fn stream_rows<'a>(&'a mut self) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError>;

    /// Commit the snapshot transaction. Call on the success path of a
    /// compile session.
    async fn commit(self: Box<Self>) -> Result<(), SourceError>;

    /// Roll back the snapshot transaction. Call on the error path of a
    /// compile session. `Drop` does not perform I/O - adapters rely on
    /// pool-level recycling for safety, so callers should still invoke
    /// `commit` or `rollback` explicitly.
    async fn rollback(self: Box<Self>) -> Result<(), SourceError>;
}

/// Per-row record returned by `Source` and `CompileSession`. Geometry is
/// opaque adapter-native bytes (typically WKB); attributes are decoded into
/// `(name, AttrValue)` pairs ordered to match the binding's projection.
///
/// `row_key` carries the snapshot-stable row identity when the row was
/// produced inside a `CompileSession`. Stateless `Source::stream_*` callers
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

/// Raster-side binding: identifies a pyramidal tile source and its native
/// addressing. Concrete adapters extend the locator semantics (XYZ URL
/// template, COG byte ranges, WMTS endpoint) - the port keeps the field
/// set minimal so the dispatch can remain backend-agnostic.
#[derive(Debug, Clone, PartialEq)]
pub struct RasterBinding {
    /// Logical collection name.
    pub collection: SourceCollectionId,
    /// Opaque backend-side locator (e.g. URL template, object-store prefix,
    /// COG key). Format is defined by the adapter.
    pub locator: String,
    /// Native source CRS of the underlying pyramid.
    pub source_crs: CrsCode,
    /// Native tile edge in pixels (typically 256 or 512). Adapters that
    /// produce variable-size tiles report their advertised default here.
    pub tile_size: u32,
    /// Maximum zoom level published by the source (inclusive).
    pub max_level: u32,
}

/// One raster tile pulled from a raster source. `bytes` carries the encoded
/// payload as the source delivered it (typically PNG / JPEG / WebP); the
/// renderer decodes lazily based on `content_type`.
#[derive(Debug, Clone)]
pub struct TileBytes {
    /// Encoded tile bytes.
    pub bytes: Bytes,
    /// IANA media type of the encoded payload, e.g. `"image/png"`.
    pub content_type: &'static str,
}

/// Read-side port for raster pyramids. Sits beside [`Source`] so each
/// adapter advertises the kind of data it produces without one trait
/// pretending to cover both.
#[async_trait]
pub trait RasterSource: Send + Sync + 'static {
    /// Read one tile from the bound pyramid. Coordinates are
    /// `(zoom, x, y)` in the source's native tiling scheme; the caller is
    /// responsible for mapping the request CRS / TMS to the source pyramid.
    async fn read_tile(&self, binding: &RasterBinding, z: u32, x: u32, y: u32) -> Result<TileBytes, SourceError>;
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
    /// - `Ok(Some(guard))` - leader; hold `guard` for the duration of work.
    /// - `Ok(None)` - another instance holds the lock.
    /// - `Err(_)` - backend error.
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
            "public.roads",
            "geom",
            "gid",
            vec!["name".into(), "class".into()],
            CrsCode::new("EPSG:25832"),
        )
        .unwrap();
        assert_eq!(b.collection.as_str(), "roads");
        assert_eq!(b.from, "public.roads");
        assert_eq!(b.geometry_field, "geom");
        assert_eq!(b.id_field, "gid");
        assert_eq!(b.attributes, vec!["name".to_string(), "class".to_string()]);
        assert_eq!(b.crs.as_str(), "EPSG:25832");
    }

    #[test]
    fn binding_rejects_duplicate_attribute() {
        let r = SourceBinding::new(
            SourceCollectionId::new("c"),
            "s.t",
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
            "g",
            "id",
            vec![],
            CrsCode::new("EPSG:4326"),
        );
        assert!(matches!(r, Err(SourceError::InvalidBinding(_))));
    }

    // phase-c will reintroduce page-keyed Source surface and its tests.
}

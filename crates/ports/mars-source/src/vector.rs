//! Vector read port: `Source`, `CompileSession`, and the row / summary
//! carriers they stream.

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;

use crate::{BindingHealth, SourceBinding, SourceCollectionId, SourceError};

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

/// Read-side port for vector sources.
///
/// Stateless `stream_*` methods drive the incremental cycle path:
/// `stream_rows` for snapshot bootstrap, `stream_rows_by_id` for per-id
/// page rebuilds, and `stream_feature_ids` for the reconciliation backstop
/// that compares the source id set against the page-membership sidecar.
/// `open_compile_session` returns a snapshot-isolated session for the
/// unified pass-1 / pass-2 compile pipeline. `probe_binding_health` is a
/// periodic backstop for bindings whose backing object can disappear
/// without an in-band change-feed signal.
///
/// Streaming is bag-valued: adapters may return multiple rows per feature
/// id, in which case the compiler treats each row as a distinct substrate
/// feature for rendering.
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

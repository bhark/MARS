//! Change-feed value types and subscription traits.
//!
//! Concrete adapters live in `crates/adapters/mars-source-*`. Adapters MUST
//! only emit a [`ChangeBatch`] once the upstream transaction has committed;
//! the `source_version` cursor lets the compiler advance manifest provenance
//! atomically per batch.

use async_trait::async_trait;
use mars_types::Bbox;

use crate::{SourceCollectionId, SourceError};

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
/// [`crate::Source::probe_binding_health`]. Adapters with no notion of
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

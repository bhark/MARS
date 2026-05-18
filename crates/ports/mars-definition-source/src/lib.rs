//! Port trait for the operator's render-definition source.
//!
//! A `DefinitionSource` is the operator-side abstraction over the four places a
//! `RenderDefinition` payload may live: inline on the CR, an in-cluster
//! `ConfigMap`, a git repository, or an S3-compatible object store. The trait
//! exposes a one-shot [`DefinitionSource::fetch`] and a [`DefinitionSource::watch`]
//! stream of [`Change`] notifications. The reconciler re-fetches on each change
//! event, so the event payload is intentionally minimal (revision only).
//!
//! Concrete adapters live in `crates/adapters/mars-definition-source-*`. This
//! crate is runtime-agnostic: no tokio, no kube, no object_store, no reqwest.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;

/// Bytes of a fetched render-definition payload, plus a backend-stable identity
/// for the fetched revision.
#[derive(Debug, Clone)]
pub struct DefinitionBytes {
    /// Raw payload bytes (typically YAML).
    pub data: Bytes,
    /// Backend-stable identity for the fetched payload: commit SHA (git), ETag
    /// (s3), ConfigMap `resourceVersion` (configmap), or a content hash prefix
    /// (inline). Surfaced into `MarsService.status.definition.observed`.
    pub revision: String,
}

/// Change notification emitted by [`DefinitionSource::watch`]. Carries only the
/// new revision identity; the reconciler does the actual re-fetch + compose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Backend-stable identity for the new payload (see [`DefinitionBytes::revision`]).
    pub revision: String,
}

/// Port trait for fetching and watching a render-definition payload.
///
/// `fetch` is one-shot; the reconciler calls it on demand. `watch` returns a
/// stream of [`Change`] events; pollers tick on the adapter's configured
/// `interval`, in-cluster watches stream native kube events, and the inline
/// adapter returns an empty stream (the CR `resourceVersion` change drives
/// reconcile via the existing kube watch on `MarsService`).
#[async_trait]
pub trait DefinitionSource: Send + Sync {
    /// Fetch the current payload and its revision identity.
    async fn fetch(&self) -> Result<DefinitionBytes, DefinitionSourceError>;

    /// Stream change notifications. Each [`Change`] carries the new revision;
    /// the reconciler re-fetches on receipt.
    fn watch(&self) -> BoxStream<'static, Change>;
}

/// Errors produced by definition-source adapters.
#[derive(Debug, thiserror::Error)]
pub enum DefinitionSourceError {
    /// The adapter does not implement this method yet (stub).
    #[error("not implemented: {what}")]
    NotImplemented {
        /// Human-readable name of the unimplemented operation.
        what: &'static str,
    },
    /// Transport / connectivity failure (network, TLS, DNS, driver). `source`
    /// carries the original error chain without forcing a port-level dep on a
    /// specific driver.
    #[error("network error: {what}")]
    Network {
        /// Stable short label for what was being attempted.
        what: &'static str,
        /// Original error chain.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
    /// The backend confirmed the requested object does not exist (404, missing
    /// ConfigMap key, missing git ref, missing S3 key).
    #[error("not found: {what}")]
    NotFound {
        /// Stable identifier of what was not found.
        what: String,
    },
    /// Authentication or authorization failure against the backend.
    #[error("auth error: {what}")]
    Auth {
        /// Stable identifier of the failure (e.g. "git basic creds rejected").
        what: String,
    },
    /// Payload was fetched but could not be decoded into a usable revision
    /// (e.g. ConfigMap key value not utf-8, git object decode failure).
    #[error("decode error: {what}")]
    Decode {
        /// Stable short label for what failed to decode.
        what: &'static str,
        /// Original error chain.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
    /// Backend-specific failure not covered by the variants above.
    #[error("other: {message}")]
    Other {
        /// Free-form operator-facing message.
        message: String,
    },
}

impl DefinitionSourceError {
    /// Build a `Network` error wrapping an existing error chain.
    pub fn network(what: &'static str, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Network {
            what,
            source: Box::new(source),
        }
    }

    /// Build a `Decode` error wrapping an existing error chain.
    pub fn decode(what: &'static str, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Decode {
            what,
            source: Box::new(source),
        }
    }
}

#[cfg(test)]
mod tests;

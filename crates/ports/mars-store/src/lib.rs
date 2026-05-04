//! port traits for artifact storage and manifest pub/sub.
//!
//! - [`ObjectStore`] - the shared, cloud-grade artifact bucket (S3 / R2 / GCS / FS).
//! - [`LocalCache`]  - the per-pod SSD cache (mirrored key layout, mmap-friendly).
//! - [`ManifestPublisher`] / [`ManifestWatch`] - atomically swap the current
//!   manifest pointer and notify subscribers (SPEC §8.5 / §10.5).
//!
//! adapters live under `crates/adapters/mars-store-*`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use mars_types::{ArtifactKey, ContentHash, Manifest};

/// Errors from the storage subsystem.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The adapter does not implement this method yet (Phase 0 stubs).
    #[error("not implemented: {what}")]
    NotImplemented {
        /// Human-readable name of the unimplemented operation.
        what: &'static str,
    },
    /// The object was not present in the store.
    #[error("object not found: {0}")]
    NotFound(ArtifactKey),
    /// Backend / transport error.
    #[error("backend error: {0}")]
    Backend(String),
    /// Content hash mismatch on read (corruption or wrong version pointer).
    #[error("content hash mismatch for {key}")]
    HashMismatch {
        /// Key whose contents did not match the expected hash.
        key: ArtifactKey,
    },
}

/// Read/write port for the shared artifact object store.
#[async_trait]
pub trait ObjectStore: Send + Sync + 'static {
    /// Fetch an object by key, verifying its content hash.
    async fn get(&self, key: &ArtifactKey, expected: ContentHash) -> Result<Bytes, StoreError>;

    /// Store an object under `key`. Returns the content hash actually written.
    async fn put(&self, key: &ArtifactKey, body: Bytes) -> Result<ContentHash, StoreError>;

    /// Delete an object.
    async fn delete(&self, key: &ArtifactKey) -> Result<(), StoreError>;

    /// List object keys under a prefix.
    async fn list(&self, prefix: &str) -> Result<Vec<ArtifactKey>, StoreError>;
}

/// Per-pod local SSD cache. Layout mirrors the object-store key layout.
#[async_trait]
pub trait LocalCache: Send + Sync + 'static {
    /// Returns the cached bytes for `key`, fetching through `origin` on miss.
    async fn get_or_fetch(
        &self,
        key: &ArtifactKey,
        expected: ContentHash,
        origin: &dyn ObjectStore,
    ) -> Result<Bytes, StoreError>;

    /// Hint that `key` is no longer needed by the live manifest.
    fn mark_evictable(&self, key: &ArtifactKey);
}

/// Publishes a new manifest atomically (SPEC §8.5).
#[async_trait]
pub trait ManifestPublisher: Send + Sync + 'static {
    /// Write the manifest body and atomically swap `manifests/current`.
    /// Returns the version that was published.
    async fn publish(&self, manifest: &Manifest) -> Result<u64, StoreError>;
}

/// Subscribes to manifest-pointer changes.
#[async_trait]
pub trait ManifestWatch: Send + Sync + 'static {
    /// Stream of `Manifest` snapshots; the stream yields the current manifest
    /// once on subscribe, then again whenever the pointer changes.
    async fn watch(&self) -> Result<BoxStream<'static, Result<Manifest, StoreError>>, StoreError>;
}

/// Reads the currently published manifest without subscribing to changes.
/// Kept decoupled from [`ManifestPublisher`] so read and write ports can be
/// wired to different backends (e.g. publisher writes to S3, reader polls
/// a local replica).
#[async_trait]
pub trait ManifestReader: Send + Sync + 'static {
    /// Returns the current manifest, or `None` if none has been published yet.
    async fn current_manifest(&self) -> Result<Option<Manifest>, StoreError>;
}

/// Phase-0 stub adapters that satisfy the port traits with `NotImplemented`.
/// Lets bins and tests compose the surface without naming a real backend.
pub mod stub {
    use super::{LocalCache, ManifestPublisher, ObjectStore, StoreError};
    use async_trait::async_trait;
    use bytes::Bytes;
    use mars_types::{ArtifactKey, ContentHash, Manifest};

    /// `ManifestPublisher` impl that always returns `NotImplemented`.
    #[derive(Debug, Default)]
    pub struct NotImplementedPublisher;

    #[async_trait]
    impl ManifestPublisher for NotImplementedPublisher {
        async fn publish(&self, _manifest: &Manifest) -> Result<u64, StoreError> {
            Err(StoreError::NotImplemented {
                what: "mars-store::stub::NotImplementedPublisher::publish",
            })
        }
    }

    /// `ObjectStore` impl that always returns `NotImplemented`. Used by bins
    /// before composition wires a real backend.
    #[derive(Debug, Default)]
    pub struct NotImplementedStore;

    #[async_trait]
    impl ObjectStore for NotImplementedStore {
        async fn get(&self, _key: &ArtifactKey, _expected: ContentHash) -> Result<Bytes, StoreError> {
            Err(StoreError::NotImplemented {
                what: "mars-store::stub::NotImplementedStore::get",
            })
        }
        async fn put(&self, _key: &ArtifactKey, _body: Bytes) -> Result<ContentHash, StoreError> {
            Err(StoreError::NotImplemented {
                what: "mars-store::stub::NotImplementedStore::put",
            })
        }
        async fn delete(&self, _key: &ArtifactKey) -> Result<(), StoreError> {
            Err(StoreError::NotImplemented {
                what: "mars-store::stub::NotImplementedStore::delete",
            })
        }
        async fn list(&self, _prefix: &str) -> Result<Vec<ArtifactKey>, StoreError> {
            Err(StoreError::NotImplemented {
                what: "mars-store::stub::NotImplementedStore::list",
            })
        }
    }

    /// `LocalCache` impl that always returns `NotImplemented`.
    #[derive(Debug, Default)]
    pub struct NotImplementedCache;

    #[async_trait]
    impl LocalCache for NotImplementedCache {
        async fn get_or_fetch(
            &self,
            _key: &ArtifactKey,
            _expected: ContentHash,
            _origin: &dyn ObjectStore,
        ) -> Result<Bytes, StoreError> {
            Err(StoreError::NotImplemented {
                what: "mars-store::stub::NotImplementedCache::get_or_fetch",
            })
        }
        fn mark_evictable(&self, _key: &ArtifactKey) {}
    }
}

/// In-memory implementations of the store ports for unit / integration tests.
/// Enabled by the `test-utils` feature or when compiling `mars-store` tests.
#[cfg(any(test, feature = "test-utils"))]
pub mod mem;

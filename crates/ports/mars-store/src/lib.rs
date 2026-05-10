//! port traits for artifact storage and manifest pub/sub.
//!
//! - [`ObjectStore`] - the shared, cloud-grade artifact bucket (S3 / R2 / GCS / FS).
//! - [`LocalCache`]  - the per-pod SSD cache (mirrored key layout, mmap-friendly).
//! - [`ManifestStore`] - publish, read, and watch the current manifest pointer
//!   (SPEC §8.5 / §10.5). The single trait collapses what used to be three
//!   sibling traits with identical impl sites.
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
    /// The adapter does not implement this method yet (stub).
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
    /// Transient backend error (network blip, throttling). Callers may retry
    /// after backoff. Adapters opt in by mapping known-retriable errors here
    /// instead of [`StoreError::Backend`].
    #[error("transient backend error: {0}")]
    Transient(String),
    /// Content hash mismatch on read (corruption or wrong version pointer).
    #[error("content hash mismatch for {key}")]
    HashMismatch {
        /// Key whose contents did not match the expected hash.
        key: ArtifactKey,
    },
    /// Manifest envelope is at a `format_version` this binary cannot decode.
    #[error("unsupported manifest format_version {found}; this binary supports {supported}")]
    UnsupportedManifestVersion {
        /// `format_version` read from the manifest body.
        found: u32,
        /// Highest `format_version` this binary understands.
        supported: u32,
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
}

/// Single port for manifest pub/sub. Replaces the older split into
/// `ManifestPublisher` / `ManifestReader` / `ManifestWatch` - every concrete
/// adapter implemented all three on the same struct, and consumers now hold
/// one `Arc<dyn ManifestStore>` instead of three separate trait objects.
#[async_trait]
pub trait ManifestStore: Send + Sync + 'static {
    /// Write the manifest body and atomically swap `manifests/current`.
    /// Returns the version that was published.
    async fn publish(&self, manifest: &Manifest) -> Result<u64, StoreError>;

    /// Returns the current manifest, or `None` if none has been published yet.
    async fn current(&self) -> Result<Option<Manifest>, StoreError>;

    /// Stream of `Manifest` snapshots; the stream yields the current manifest
    /// once on subscribe, then again whenever the pointer changes.
    async fn watch(&self) -> Result<BoxStream<'static, Result<Manifest, StoreError>>, StoreError>;
}

/// Phase-0 stub adapters that satisfy the port traits with `NotImplemented`.
/// Lets bins and tests compose the surface without naming a real backend.
pub mod stub {
    use super::{LocalCache, ManifestStore, ObjectStore, StoreError};
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures_core::stream::BoxStream;
    use futures_util::stream;
    use mars_types::{ArtifactKey, ContentHash, Manifest};

    /// `ManifestStore` impl that always returns `NotImplemented`.
    #[derive(Debug, Default)]
    pub struct NotImplementedManifestStore;

    #[async_trait]
    impl ManifestStore for NotImplementedManifestStore {
        async fn publish(&self, _manifest: &Manifest) -> Result<u64, StoreError> {
            Err(StoreError::NotImplemented {
                what: "mars-store::stub::NotImplementedManifestStore::publish",
            })
        }
        async fn current(&self) -> Result<Option<Manifest>, StoreError> {
            Err(StoreError::NotImplemented {
                what: "mars-store::stub::NotImplementedManifestStore::current",
            })
        }
        async fn watch(&self) -> Result<BoxStream<'static, Result<Manifest, StoreError>>, StoreError> {
            // empty stream is valid for a stub: consumers will simply observe
            // no manifest swaps.
            Ok(Box::pin(stream::empty()))
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
    }
}

/// In-memory implementations of the store ports for unit / integration tests.
/// Enabled by the `test-utils` feature or when compiling `mars-store` tests.
#[cfg(any(test, feature = "test-utils"))]
pub mod mem;

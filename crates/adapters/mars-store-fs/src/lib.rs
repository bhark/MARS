//! filesystem-backed adapter for `mars-store::ObjectStore`. also provides the
//! node-local SSD `LocalCache` since SPEC §10.3 specifies the cache layout
//! mirrors the object-store key layout.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use async_trait::async_trait;
use bytes::Bytes;
use mars_store::{LocalCache, ObjectStore, StoreError};
use mars_types::{ArtifactKey, ContentHash};

/// Local-filesystem store / cache configuration.
#[derive(Debug, Clone, Default)]
pub struct FsConfig {
    /// Root directory under which all artifact files live.
    pub root: PathBuf,
    /// Maximum cache size in bytes; `0` means unbounded (only valid when used
    /// as the authoritative store, not as a cache).
    pub max_size_bytes: u64,
}

#[derive(Debug, Default)]
pub struct StubFs {
    _cfg: FsConfig,
}

impl StubFs {
    #[must_use]
    pub fn new(cfg: FsConfig) -> Self {
        Self { _cfg: cfg }
    }
}

#[async_trait]
impl ObjectStore for StubFs {
    async fn get(&self, _key: &ArtifactKey, _expected: ContentHash) -> Result<Bytes, StoreError> {
        // todo(SPEC §10.2 / §10.3) read file by content-addressed key
        Err(StoreError::NotImplemented {
            what: "mars-store-fs::ObjectStore::get",
        })
    }
    async fn put(&self, _key: &ArtifactKey, _body: Bytes) -> Result<ContentHash, StoreError> {
        Err(StoreError::NotImplemented {
            what: "mars-store-fs::ObjectStore::put",
        })
    }
    async fn delete(&self, _key: &ArtifactKey) -> Result<(), StoreError> {
        Err(StoreError::NotImplemented {
            what: "mars-store-fs::ObjectStore::delete",
        })
    }
    async fn list(&self, _prefix: &str) -> Result<Vec<ArtifactKey>, StoreError> {
        Err(StoreError::NotImplemented {
            what: "mars-store-fs::ObjectStore::list",
        })
    }
}

#[async_trait]
impl LocalCache for StubFs {
    async fn get_or_fetch(
        &self,
        _key: &ArtifactKey,
        _expected: ContentHash,
        _origin: &dyn ObjectStore,
    ) -> Result<Bytes, StoreError> {
        Err(StoreError::NotImplemented {
            what: "mars-store-fs::LocalCache::get_or_fetch",
        })
    }
    fn mark_evictable(&self, _key: &ArtifactKey) {}
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stub_returns_not_implemented() {
        let s = StubFs::default();
        let r = s.list("").await;
        assert!(matches!(r, Err(StoreError::NotImplemented { .. })));
    }
}

//! S3 / MinIO / R2 / GCS adapter for `mars-store::ObjectStore`. Real
//! implementation uses the `object_store` crate and is wired in Phase 1.

#![forbid(unsafe_code)]

use async_trait::async_trait;
use bytes::Bytes;
use mars_store::{ObjectStore, StoreError};
use mars_types::{ArtifactKey, ContentHash};

/// Connection / topology configuration. Filled in during Phase 1.
#[derive(Debug, Clone, Default)]
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub prefix: String,
}

#[derive(Debug, Default)]
pub struct StubS3 {
    _cfg: S3Config,
}

impl StubS3 {
    #[must_use]
    pub fn new(cfg: S3Config) -> Self {
        Self { _cfg: cfg }
    }
}

#[async_trait]
impl ObjectStore for StubS3 {
    async fn get(&self, _key: &ArtifactKey, _expected: ContentHash) -> Result<Bytes, StoreError> {
        // todo(SPEC §10.2) wire object_store::aws
        Err(StoreError::NotImplemented {
            what: "mars-store-s3::ObjectStore::get",
        })
    }
    async fn put(&self, _key: &ArtifactKey, _body: Bytes) -> Result<ContentHash, StoreError> {
        Err(StoreError::NotImplemented {
            what: "mars-store-s3::ObjectStore::put",
        })
    }
    async fn delete(&self, _key: &ArtifactKey) -> Result<(), StoreError> {
        Err(StoreError::NotImplemented {
            what: "mars-store-s3::ObjectStore::delete",
        })
    }
    async fn list(&self, _prefix: &str) -> Result<Vec<ArtifactKey>, StoreError> {
        Err(StoreError::NotImplemented {
            what: "mars-store-s3::ObjectStore::list",
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stub_returns_not_implemented() {
        let s = StubS3::default();
        let r = s.list("").await;
        assert!(matches!(r, Err(StoreError::NotImplemented { .. })));
    }
}

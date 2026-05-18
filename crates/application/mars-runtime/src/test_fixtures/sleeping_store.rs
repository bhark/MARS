//! `ObjectStore` decorator that injects a per-key sleep on `get`. used in
//! integration tests to skew per-layer page-fetch completion order so the
//! FuturesUnordered reassembly step is forced to reorder.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use mars_store::ObjectStore;
use mars_store::StoreError;
use mars_types::{ArtifactKey, ContentHash};

pub struct SleepingStore {
    inner: Arc<dyn ObjectStore>,
    delays: HashMap<ArtifactKey, Duration>,
}

impl SleepingStore {
    pub fn new(inner: Arc<dyn ObjectStore>, delays: HashMap<ArtifactKey, Duration>) -> Self {
        Self { inner, delays }
    }
}

#[async_trait]
impl ObjectStore for SleepingStore {
    async fn get(&self, key: &ArtifactKey, expected: ContentHash) -> Result<Bytes, StoreError> {
        if let Some(d) = self.delays.get(key) {
            tokio::time::sleep(*d).await;
        }
        self.inner.get(key, expected).await
    }
    async fn put(&self, key: &ArtifactKey, body: Bytes) -> Result<ContentHash, StoreError> {
        self.inner.put(key, body).await
    }
    async fn delete(&self, key: &ArtifactKey) -> Result<(), StoreError> {
        self.inner.delete(key).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<ArtifactKey>, StoreError> {
        self.inner.list(prefix).await
    }
}

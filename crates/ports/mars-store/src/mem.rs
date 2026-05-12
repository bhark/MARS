//! In-memory implementations of [`ObjectStore`], [`LocalCache`], and
//! [`ManifestStore`] for use in unit / integration tests.
//!
//! These types avoid pulling concrete filesystem or network adapters into
//! application-layer tests, keeping the test surface aligned with the port
//! traits.

// test-utils fake; poisoning means a prior test panicked while holding the
// lock - propagate that as a panic here too, no recovery path is meaningful.
#![allow(clippy::expect_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::stream;
use mars_types::{ArtifactKey, ContentHash, Manifest};

use crate::{LocalCache, ManifestStore, ObjectStore, StoreError};

fn compute_hash(bytes: &[u8]) -> ContentHash {
    let hash = blake3::hash(bytes);
    ContentHash(hash.into())
}

/// In-memory [`ObjectStore`]. Backed by a `HashMap` protected by a `Mutex`.
#[derive(Debug, Default)]
pub struct InMemoryStore {
    data: Mutex<HashMap<ArtifactKey, (Bytes, ContentHash)>>,
}

impl InMemoryStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ObjectStore for InMemoryStore {
    async fn get(&self, key: &ArtifactKey, expected: ContentHash) -> Result<Bytes, StoreError> {
        let lock = self.data.lock().expect("mem store mutex poisoned");
        let (bytes, hash) = lock.get(key).ok_or_else(|| StoreError::NotFound(key.clone()))?;
        if *hash != expected {
            return Err(StoreError::HashMismatch { key: key.clone() });
        }
        Ok(bytes.clone())
    }

    async fn put(&self, key: &ArtifactKey, body: Bytes) -> Result<ContentHash, StoreError> {
        let hash = compute_hash(&body);
        let mut lock = self.data.lock().expect("mem store mutex poisoned");
        lock.insert(key.clone(), (body, hash));
        Ok(hash)
    }

    async fn delete(&self, key: &ArtifactKey) -> Result<(), StoreError> {
        let mut lock = self.data.lock().expect("mem store mutex poisoned");
        lock.remove(key);
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ArtifactKey>, StoreError> {
        let lock = self.data.lock().expect("mem store mutex poisoned");
        let keys: Vec<_> = lock
            .keys()
            .filter(|k| k.as_str().starts_with(prefix))
            .cloned()
            .collect();
        Ok(keys)
    }
}

/// In-memory [`LocalCache`]. Simply delegates to an inner [`ObjectStore`]-like
/// map; no eviction logic (tests are short-lived).
#[derive(Debug, Default, Clone)]
pub struct InMemoryCache {
    data: Arc<Mutex<HashMap<ArtifactKey, Bytes>>>,
}

impl InMemoryCache {
    /// Create an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl LocalCache for InMemoryCache {
    async fn get_or_fetch(
        &self,
        key: &ArtifactKey,
        expected: ContentHash,
        origin: &dyn ObjectStore,
    ) -> Result<Bytes, StoreError> {
        {
            let lock = self.data.lock().expect("mem store mutex poisoned");
            if let Some(bytes) = lock.get(key) {
                // verify hash on cached entry
                let actual = compute_hash(bytes);
                if actual == expected {
                    return Ok(bytes.clone());
                }
            }
        }
        let bytes = origin.get(key, expected).await?;
        let mut lock = self.data.lock().expect("mem store mutex poisoned");
        lock.insert(key.clone(), bytes.clone());
        Ok(bytes)
    }
}

/// In-memory [`ManifestStore`]. Records the latest published manifest; `watch`
/// emits a one-shot stream of the current value (tests rebuild the watch when
/// they want to observe further updates).
#[derive(Debug, Default)]
pub struct InMemoryPublisher {
    manifest: Mutex<Option<Manifest>>,
}

impl InMemoryPublisher {
    /// Create an empty publisher.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ManifestStore for InMemoryPublisher {
    async fn publish(&self, manifest: &Manifest) -> Result<u64, StoreError> {
        let mut lock = self.manifest.lock().expect("mem store mutex poisoned");
        *lock = Some(manifest.clone());
        Ok(manifest.version)
    }

    async fn current(&self) -> Result<Option<Manifest>, StoreError> {
        let lock = self.manifest.lock().expect("mem store mutex poisoned");
        Ok(lock.clone())
    }

    async fn watch(&self) -> Result<BoxStream<'static, Result<Manifest, StoreError>>, StoreError> {
        let snapshot = self.current().await?;
        let items: Vec<Result<Manifest, StoreError>> = snapshot.into_iter().map(Ok).collect();
        Ok(Box::pin(stream::iter(items)))
    }
}

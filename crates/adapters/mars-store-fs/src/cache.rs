//! filesystem-backed [`LocalCache`]. mirrors the object-store key layout.
//!
//! phase-0: no LRU eviction. cap is part of config but not enforced; phase-1
//! tightens this. on miss or hash-mismatch we transparently re-fetch from the
//! origin and write the bytes into the cache via atomic rename.

use std::path::{Path, PathBuf};
use std::sync::Once;

use async_trait::async_trait;
use bytes::Bytes;
use mars_artifact::compute_content_hash;
use mars_store::{LocalCache, ObjectStore, StoreError};
use mars_types::{ArtifactKey, ContentHash};

use crate::key::validate_artifact_key;
use crate::store::atomic_write;

static EVICTION_WARN: Once = Once::new();

/// Filesystem-backed local cache. Key layout mirrors the object store.
#[derive(Debug, Clone)]
pub struct FsCache {
    root: PathBuf,
}

impl FsCache {
    /// Open / create a cache rooted at `root`. Path is canonicalised.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let raw = root.into();
        if !raw.exists() {
            std::fs::create_dir_all(&raw)
                .map_err(|e| StoreError::Backend(format!("create cache root: {e}")))?;
        }
        let root = raw
            .canonicalize()
            .map_err(|e| StoreError::Backend(format!("canonicalise cache root: {e}")))?;
        EVICTION_WARN.call_once(|| {
            tracing::warn!("phase-1: cache eviction not yet enforced");
        });
        Ok(Self { root })
    }

    /// Canonical, absolute root path.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[async_trait]
impl LocalCache for FsCache {
    async fn get_or_fetch(
        &self,
        key: &ArtifactKey,
        expected: ContentHash,
        origin: &dyn ObjectStore,
    ) -> Result<Bytes, StoreError> {
        let path = validate_artifact_key(&self.root, key)?;

        // try local first; treat NotFound and HashMismatch as miss.
        let local = {
            let p = path.clone();
            tokio::task::spawn_blocking(move || -> Result<Option<Bytes>, StoreError> {
                match std::fs::read(&p) {
                    Ok(b) => {
                        let actual = compute_content_hash(&b);
                        if actual == expected {
                            Ok(Some(Bytes::from(b)))
                        } else {
                            Ok(None)
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(StoreError::Backend(format!("cache read: {e}"))),
                }
            })
            .await
            .map_err(|e| StoreError::Backend(format!("join: {e}")))??
        };

        if let Some(bytes) = local {
            return Ok(bytes);
        }

        // miss: fetch from origin and persist.
        let bytes = origin.get(key, expected).await?;
        let body = bytes.clone();
        tokio::task::spawn_blocking(move || atomic_write(&path, &body))
            .await
            .map_err(|e| StoreError::Backend(format!("join: {e}")))??;
        Ok(bytes)
    }

    fn mark_evictable(&self, _key: &ArtifactKey) {
        // phase-0 no-op; phase-1 LRU will consume this hint.
    }
}

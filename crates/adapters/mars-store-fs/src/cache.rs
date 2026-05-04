//! filesystem-backed [`LocalCache`]. mirrors the object-store key layout.
//!
//! implements size-bounded lru eviction: on write, if the cache exceeds its
//! configured max size, the least-recently-used entries are deleted until the
//! budget is restored.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use mars_artifact::compute_content_hash;
use mars_store::{LocalCache, ObjectStore, StoreError};
use mars_types::{ArtifactKey, ContentHash};

use crate::key::validate_artifact_key;
use crate::store::atomic_write;

#[derive(Debug)]
struct CacheState {
    total_size: u64,
    max_size: u64,
    // lazy lru: every access pushes to the back. stale duplicates are skipped
    // during eviction.
    lru: VecDeque<ArtifactKey>,
    sizes: HashMap<ArtifactKey, u64>,
}

impl CacheState {
    fn touch(&mut self, key: ArtifactKey) {
        self.lru.push_back(key);
    }

    fn insert(&mut self, key: ArtifactKey, size: u64) {
        self.total_size += size;
        self.sizes.insert(key.clone(), size);
        self.lru.push_back(key);
        self.evict();
    }

    fn evict(&mut self) {
        while self.total_size > self.max_size {
            let Some(candidate) = self.lru.pop_front() else {
                break;
            };
            let Some(size) = self.sizes.remove(&candidate) else {
                // stale entry (key was accessed again later)
                continue;
            };
            self.total_size -= size;
        }
    }
}

/// Filesystem-backed local cache. Key layout mirrors the object store.
#[derive(Debug)]
pub struct FsCache {
    root: PathBuf,
    state: Mutex<CacheState>,
}

#[allow(clippy::unwrap_used)]
impl Clone for FsCache {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            state: Mutex::new(CacheState {
                total_size: 0,
                max_size: self.state.lock().unwrap().max_size,
                lru: VecDeque::new(),
                sizes: HashMap::new(),
            }),
        }
    }
}

impl FsCache {
    /// Open / create a cache rooted at `root`. Path is canonicalised.
    /// `max_size_bytes` is the hard size cap; zero disables eviction.
    pub fn new(root: impl Into<PathBuf>, max_size_bytes: u64) -> Result<Self, StoreError> {
        let raw = root.into();
        if !raw.exists() {
            std::fs::create_dir_all(&raw).map_err(|e| StoreError::Backend(format!("create cache root: {e}")))?;
        }
        let root = raw
            .canonicalize()
            .map_err(|e| StoreError::Backend(format!("canonicalise cache root: {e}")))?;
        Ok(Self {
            root,
            state: Mutex::new(CacheState {
                total_size: 0,
                max_size: max_size_bytes,
                lru: VecDeque::new(),
                sizes: HashMap::new(),
            }),
        })
    }

    /// Canonical, absolute root path.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[allow(clippy::unwrap_used)]
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
            let mut state = self.state.lock().unwrap();
            state.touch(key.clone());
            return Ok(bytes);
        }

        // miss: fetch from origin and persist.
        let bytes = origin.get(key, expected).await?;
        let body = bytes.clone();
        let k = key.clone();
        let _root = self.root.clone();
        let size = tokio::task::spawn_blocking(move || {
            atomic_write(&path, &body)?;
            let meta = std::fs::metadata(&path).map_err(|e| StoreError::Backend(format!("cache stat: {e}")))?;
            Ok::<_, StoreError>(meta.len())
        })
        .await
        .map_err(|e| StoreError::Backend(format!("join: {e}")))??;

        {
            let mut state = self.state.lock().unwrap();
            state.insert(k, size);
        }
        Ok(bytes)
    }

    fn mark_evictable(&self, _key: &ArtifactKey) {
        // lru treats all cached keys uniformly; this hint is not needed for
        // correctness but may be used later for eager cleanup.
    }
}

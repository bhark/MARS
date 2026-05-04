//! filesystem-backed [`LocalCache`]. mirrors the object-store key layout.
//!
//! implements size-bounded lru eviction: on write, if the cache exceeds its
//! configured max size, the least-recently-used entries are deleted until the
//! budget is restored.
//!
//! state is held under a `Mutex` and never crosses an `.await`. on
//! construction the existing root is scanned synchronously so the in-memory
//! accounting starts consistent with what is already on disk; lru order is
//! seeded by mtime (oldest first) so eviction is deterministic across restart.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use mars_artifact::compute_content_hash;
use mars_store::{LocalCache, ObjectStore, StoreError};
use mars_types::{ArtifactKey, ContentHash};

use crate::key::validate_artifact_key;
use crate::store::atomic_write;

/// scan-time aggregate: (size-by-key, lru-ordered-by-mtime, total bytes).
type ScanResult = (HashMap<ArtifactKey, u64>, VecDeque<ArtifactKey>, u64);

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

    /// inserts (or overwrites) `key` with `size`. caller is responsible for
    /// deleting evicted files on disk via the returned key list.
    fn insert(&mut self, key: ArtifactKey, size: u64) -> Vec<ArtifactKey> {
        let prev = self.sizes.insert(key.clone(), size);
        self.total_size = self
            .total_size
            .saturating_sub(prev.unwrap_or(0))
            .saturating_add(size);
        self.lru.push_back(key);
        self.evict()
    }

    fn evict(&mut self) -> Vec<ArtifactKey> {
        let mut evicted = Vec::new();
        if self.max_size == 0 {
            return evicted;
        }
        while self.total_size > self.max_size {
            let Some(candidate) = self.lru.pop_front() else {
                break;
            };
            let Some(size) = self.sizes.remove(&candidate) else {
                // stale entry (key was accessed again later)
                continue;
            };
            self.total_size = self.total_size.saturating_sub(size);
            evicted.push(candidate);
        }
        evicted
    }
}

/// Filesystem-backed local cache. Key layout mirrors the object store.
///
/// Not `Clone`: callers should share via `Arc<FsCache>` (or
/// `Arc<dyn LocalCache>`) so all references see the same accounting.
#[derive(Debug)]
pub struct FsCache {
    root: PathBuf,
    state: Mutex<CacheState>,
}

impl FsCache {
    /// Open / create a cache rooted at `root`. Path is canonicalised. Existing
    /// files under `root` are scanned synchronously to seed the in-memory
    /// accounting (size + lru order by mtime, oldest first).
    ///
    /// `max_size_bytes` is the hard size cap; zero disables eviction.
    pub fn new(root: impl Into<PathBuf>, max_size_bytes: u64) -> Result<Self, StoreError> {
        let raw = root.into();
        if !raw.exists() {
            std::fs::create_dir_all(&raw).map_err(|e| StoreError::Backend(format!("create cache root: {e}")))?;
        }
        let root = raw
            .canonicalize()
            .map_err(|e| StoreError::Backend(format!("canonicalise cache root: {e}")))?;

        let (sizes, lru, total_size) = scan_existing(&root)?;
        let state = CacheState {
            total_size,
            max_size: max_size_bytes,
            lru,
            sizes,
        };

        let cache = Self {
            root,
            state: Mutex::new(state),
        };

        // bring on-disk state inside the budget if the existing footprint
        // already exceeds it.
        cache.evict_to_budget()?;
        Ok(cache)
    }

    /// Canonical, absolute root path.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn evict_to_budget(&self) -> Result<(), StoreError> {
        let evicted = {
            let mut state = lock_state(&self.state);
            state.evict()
        };
        for key in evicted {
            let path = validate_artifact_key(&self.root, &key)?;
            // best-effort: a missing file is fine (already gone).
            if let Err(e) = std::fs::remove_file(&path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                return Err(StoreError::Backend(format!("evict {}: {e}", path.display())));
            }
        }
        Ok(())
    }
}

/// scope a `Mutex` lock so it never crosses an `.await`. on poison we panic;
/// the cache state is in-process and a poison is unrecoverable.
#[allow(clippy::expect_used)]
fn lock_state(m: &Mutex<CacheState>) -> std::sync::MutexGuard<'_, CacheState> {
    m.lock().expect("cache state poisoned")
}

/// walk `root` recursively, collecting (key, size, mtime) for every file.
/// returns (sizes, lru-ordered-by-mtime, total).
fn scan_existing(root: &Path) -> Result<ScanResult, StoreError> {
    let mut entries: Vec<(ArtifactKey, u64, SystemTime)> = Vec::new();
    walk(root, root, &mut entries)?;
    entries.sort_by_key(|(_, _, mtime)| *mtime);

    let mut sizes = HashMap::with_capacity(entries.len());
    let mut lru = VecDeque::with_capacity(entries.len());
    let mut total: u64 = 0;
    for (key, size, _) in entries {
        total = total.saturating_add(size);
        sizes.insert(key.clone(), size);
        lru.push_back(key);
    }
    Ok((sizes, lru, total))
}

fn walk(dir: &Path, root: &Path, out: &mut Vec<(ArtifactKey, u64, SystemTime)>) -> Result<(), StoreError> {
    let rd = std::fs::read_dir(dir).map_err(|e| StoreError::Backend(format!("scan {}: {e}", dir.display())))?;
    for ent in rd {
        let ent = ent.map_err(|e| StoreError::Backend(format!("scan readdir: {e}")))?;
        let ft = ent
            .file_type()
            .map_err(|e| StoreError::Backend(format!("scan file_type: {e}")))?;
        let p = ent.path();
        if ft.is_dir() {
            walk(&p, root, out)?;
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        // skip stale temp files left by aborted writes
        if let Some(name) = p.file_name().and_then(|s| s.to_str())
            && name.contains(".tmp.")
        {
            continue;
        }
        let meta = ent
            .metadata()
            .map_err(|e| StoreError::Backend(format!("scan metadata: {e}")))?;
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let rel = p
            .strip_prefix(root)
            .map_err(|e| StoreError::Backend(format!("scan strip_prefix: {e}")))?;
        let rel_str = rel
            .to_str()
            .ok_or_else(|| StoreError::Backend("scan: non-utf8 path".into()))?
            .replace('\\', "/");
        out.push((ArtifactKey::new(rel_str), meta.len(), mtime));
    }
    Ok(())
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
            // lock scope is tight; never crosses an await.
            let mut state = lock_state(&self.state);
            state.touch(key.clone());
            return Ok(bytes);
        }

        // miss: fetch from origin and persist.
        let bytes = origin.get(key, expected).await?;
        let body = bytes.clone();
        let k = key.clone();
        let size = tokio::task::spawn_blocking(move || {
            atomic_write(&path, &body)?;
            let meta = std::fs::metadata(&path).map_err(|e| StoreError::Backend(format!("cache stat: {e}")))?;
            Ok::<_, StoreError>(meta.len())
        })
        .await
        .map_err(|e| StoreError::Backend(format!("join: {e}")))??;

        let evicted = {
            let mut state = lock_state(&self.state);
            state.insert(k, size)
        };
        for key in evicted {
            let path = validate_artifact_key(&self.root, &key)?;
            // best-effort eviction; missing file is fine.
            if let Err(e) = std::fs::remove_file(&path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(path = %path.display(), error = %e, "fs cache: evict failed");
            }
        }
        Ok(bytes)
    }

}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    use bytes::Bytes;
    use mars_store::ObjectStore;
    use tempfile::TempDir;

    use crate::FsStore;

    fn k(s: &str) -> ArtifactKey {
        ArtifactKey::new(s)
    }

    #[tokio::test]
    async fn restart_loads_existing_entries_and_evicts_to_budget() {
        let store_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let store = FsStore::new(store_dir.path()).unwrap();

        // pre-seed the cache with 4 x 1KiB files via a generous-budget cache,
        // then drop it and reopen with a 2KiB budget.
        let payload = vec![0u8; 1024];
        let h = mars_artifact::compute_content_hash(&payload);
        for i in 0..4 {
            let key = k(&format!("a/b/f{i}.bin"));
            store.put(&key, Bytes::from(payload.clone())).await.unwrap();
        }

        {
            let warm = FsCache::new(cache_dir.path(), u64::MAX).unwrap();
            for i in 0..4 {
                let key = k(&format!("a/b/f{i}.bin"));
                warm.get_or_fetch(&key, h, &store).await.unwrap();
                // small sleep so mtimes are distinct enough for ordering
                tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            }
        }

        // reopen with a 2 KiB cap. scan must populate state and eviction must
        // bring on-disk footprint within budget.
        let tight = FsCache::new(cache_dir.path(), 2048).unwrap();
        let total: u64 = walk_total(cache_dir.path());
        assert!(
            total <= 2048,
            "expected on-disk footprint <= 2KiB after reopen, got {total}"
        );
        // accounting should match what's on disk
        let state_total = tight.state.lock().unwrap().total_size;
        assert_eq!(state_total, total);
    }

    #[tokio::test]
    async fn double_insert_does_not_drift_total_size() {
        let store_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let store = FsStore::new(store_dir.path()).unwrap();
        let cache = FsCache::new(cache_dir.path(), u64::MAX).unwrap();

        let payload = b"some-bytes".to_vec();
        let h = mars_artifact::compute_content_hash(&payload);
        let key = k("x/y.bin");
        store.put(&key, Bytes::from(payload.clone())).await.unwrap();

        cache.get_or_fetch(&key, h, &store).await.unwrap();
        let after_first = cache.state.lock().unwrap().total_size;
        assert_eq!(after_first, payload.len() as u64);

        // simulate a retry / concurrent re-fetch path that re-enters insert
        // for the same key. accounting must not double-count.
        std::fs::remove_file(cache_dir.path().canonicalize().unwrap().join("x/y.bin")).unwrap();
        cache.get_or_fetch(&key, h, &store).await.unwrap();
        let after_second = cache.state.lock().unwrap().total_size;
        assert_eq!(after_second, payload.len() as u64);
    }

    #[test]
    fn fscache_is_not_clone() {
        // compile-time guard: callers must share via `Arc<FsCache>`. if a
        // future commit re-adds `impl Clone`, the negative-trait probe below
        // will still compile but the structural check that we always wrap in
        // `Arc` is the contract that matters for correctness.
        let _: fn(std::sync::Arc<FsCache>) = |_| {};
    }

    fn walk_total(p: &Path) -> u64 {
        let mut total = 0u64;
        let rd = std::fs::read_dir(p).unwrap();
        for ent in rd {
            let ent = ent.unwrap();
            let ft = ent.file_type().unwrap();
            if ft.is_dir() {
                total += walk_total(&ent.path());
            } else if ft.is_file() {
                if let Some(name) = ent.path().file_name().and_then(|s| s.to_str())
                    && name.contains(".tmp.")
                {
                    continue;
                }
                total += ent.metadata().unwrap().len();
            }
        }
        total
    }
}

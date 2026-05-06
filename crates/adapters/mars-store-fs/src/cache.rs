//! filesystem-backed [`LocalCache`]. mirrors the object-store key layout.
//!
//! three behaviours layered on top of an on-disk store:
//!
//! 1. **single-flight**: concurrent `get_or_fetch` calls for the same key
//!    coalesce into a single origin fetch. waiters block on a per-key
//!    `Notify`; on wake they retry the local read (now populated on success)
//!    or contend for a fresh leadership slot if the leader failed.
//! 2. **size budget + lru eviction**: state is held under a `Mutex` and never
//!    crosses an `.await`. on construction the existing root is scanned so
//!    the in-memory accounting starts consistent with what is on disk; lru
//!    order is seeded by mtime (oldest first).
//! 3. **mmap on read**: cached files are mapped into memory via `memmap2`
//!    and surfaced as `bytes::Bytes` through `Bytes::from_owner`. zero-copy
//!    for downstream codecs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use parking_lot::Mutex;

use linked_hash_map::LinkedHashMap;

use async_trait::async_trait;
use bytes::Bytes;
use mars_artifact::compute_content_hash;
use mars_store::{LocalCache, ObjectStore, StoreError};
use mars_types::{ArtifactKey, ContentHash};
use tokio::sync::Notify;

use crate::key::validate_artifact_key;
use crate::mmap::read_mmap;
use crate::store::{atomic_write, cleanup_tmp_files};

/// scan-time aggregate: (lru-ordered-by-mtime, total bytes).
type ScanResult = (LinkedHashMap<ArtifactKey, u64>, u64);

#[derive(Debug)]
struct CacheState {
    total_size: u64,
    max_size: u64,
    lru: LinkedHashMap<ArtifactKey, u64>,
}

impl CacheState {
    fn touch(&mut self, key: ArtifactKey) {
        // refresh position to back if already present
        let _ = self.lru.get_refresh(&key);
    }

    /// inserts (or overwrites) `key` with `size`. caller is responsible for
    /// deleting evicted files on disk via the returned key list.
    fn insert(&mut self, key: ArtifactKey, size: u64) -> Vec<ArtifactKey> {
        let prev = self.lru.insert(key, size);
        self.total_size = self.total_size.saturating_sub(prev.unwrap_or(0)).saturating_add(size);
        self.evict()
    }

    fn evict(&mut self) -> Vec<ArtifactKey> {
        let mut evicted = Vec::new();
        if self.max_size == 0 {
            return evicted;
        }
        while self.total_size > self.max_size {
            let Some((candidate, size)) = self.lru.pop_front() else {
                break;
            };
            self.total_size = self.total_size.saturating_sub(size);
            evicted.push(candidate);
        }
        evicted
    }
}

/// per-key in-flight registration. notifies all waiters when the leader
/// either persists the artifact or returns.
type Flights = Mutex<HashMap<ArtifactKey, Arc<Notify>>>;

/// Filesystem-backed local cache. Key layout mirrors the object store.
///
/// Not `Clone`: callers should share via `Arc<FsCache>` (or
/// `Arc<dyn LocalCache>`) so all references see the same accounting.
#[derive(Debug)]
pub struct FsCache {
    root: PathBuf,
    state: Mutex<CacheState>,
    flights: Flights,
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
        cleanup_tmp_files(&root)?;

        let (lru, total_size) = scan_existing(&root)?;
        let state = CacheState {
            total_size,
            max_size: max_size_bytes,
            lru,
        };

        let cache = Self {
            root,
            state: Mutex::new(state),
            flights: Mutex::new(HashMap::new()),
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
            let mut state = self.state.lock();
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

    /// register the current task as the flight leader for `key` if no flight
    /// is active, otherwise return a handle to await the existing leader.
    fn join_or_lead(&self, key: &ArtifactKey) -> FlightRole {
        let mut flights = self.flights.lock();
        if let Some(notify) = flights.get(key) {
            return FlightRole::Waiter(notify.clone());
        }
        let notify = Arc::new(Notify::new());
        flights.insert(key.clone(), notify.clone());
        FlightRole::Leader
    }

    /// unregister the flight and wake every waiter. always called by the
    /// leader, including on error and on panic (via `LeaderGuard`).
    fn finish_flight(&self, key: &ArtifactKey) {
        let notify = {
            let mut flights = self.flights.lock();
            flights.remove(key)
        };
        if let Some(n) = notify {
            n.notify_waiters();
        }
    }
}

enum FlightRole {
    Leader,
    Waiter(Arc<Notify>),
}

/// drop-guard that finishes the flight if the leader bails out by panic or
/// early return without an explicit `finish_flight` call.
struct LeaderGuard<'a> {
    cache: &'a FsCache,
    key: &'a ArtifactKey,
    armed: bool,
}

impl Drop for LeaderGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.cache.finish_flight(self.key);
        }
    }
}

/// walk `root` recursively, collecting (key, size, mtime) for every file.
/// returns (sizes, lru-ordered-by-mtime, total).
fn scan_existing(root: &Path) -> Result<ScanResult, StoreError> {
    let mut entries: Vec<(ArtifactKey, u64, SystemTime)> = Vec::new();
    walk(root, root, &mut entries)?;
    entries.sort_by_key(|(_, _, mtime)| *mtime);

    let mut lru = LinkedHashMap::new();
    let mut total: u64 = 0;
    for (key, size, _) in entries {
        total = total.saturating_add(size);
        lru.insert(key, size);
    }
    Ok((lru, total))
}

fn walk(dir: &Path, root: &Path, out: &mut Vec<(ArtifactKey, u64, SystemTime)>) -> Result<(), StoreError> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = std::fs::read_dir(&dir).map_err(|e| StoreError::Backend(format!("scan {}: {e}", dir.display())))?;
        for ent in rd {
            let ent = ent.map_err(|e| StoreError::Backend(format!("scan readdir: {e}")))?;
            let ft = ent
                .file_type()
                .map_err(|e| StoreError::Backend(format!("scan file_type: {e}")))?;
            let p = ent.path();
            if ft.is_dir() {
                stack.push(p);
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

        loop {
            // try local first; treat NotFound and HashMismatch as miss.
            let local = {
                let p = path.clone();
                tokio::task::spawn_blocking(move || -> Result<Option<Bytes>, StoreError> {
                    match read_mmap(&p)? {
                        None => Ok(None),
                        Some(bytes) => {
                            if compute_content_hash(&bytes) == expected {
                                Ok(Some(bytes))
                            } else {
                                Ok(None)
                            }
                        }
                    }
                })
                .await
                .map_err(|e| StoreError::Backend(format!("join: {e}")))??
            };

            if let Some(bytes) = local {
                let mut state = self.state.lock();
                state.touch(key.clone());
                return Ok(bytes);
            }

            // miss: contend for the single-flight slot.
            match self.join_or_lead(key) {
                FlightRole::Waiter(notify) => {
                    // leader is fetching; wait then retry from local read.
                    notify.notified().await;
                    continue;
                }
                FlightRole::Leader => {
                    // panic-safe slot release: guard's drop unregisters the
                    // flight if the future is cancelled or origin.get panics.
                    let mut guard = LeaderGuard {
                        cache: self,
                        key,
                        armed: true,
                    };
                    let res = self.leader_fetch(key, expected, origin, &path).await;
                    // explicit cleanup so all waiters wake before the guard
                    // would race; disarm so drop is a no-op.
                    self.finish_flight(key);
                    guard.armed = false;
                    return res;
                }
            }
        }
    }
}

impl FsCache {
    async fn leader_fetch(
        &self,
        key: &ArtifactKey,
        expected: ContentHash,
        origin: &dyn ObjectStore,
        path: &Path,
    ) -> Result<Bytes, StoreError> {
        let bytes = origin.get(key, expected).await?;
        let body = bytes.clone();
        let path_owned = path.to_path_buf();
        let size = tokio::task::spawn_blocking(move || {
            atomic_write(&path_owned, &body)?;
            let meta = std::fs::metadata(&path_owned).map_err(|e| StoreError::Backend(format!("cache stat: {e}")))?;
            Ok::<_, StoreError>(meta.len())
        })
        .await
        .map_err(|e| StoreError::Backend(format!("join: {e}")))??;

        let evicted = {
            let mut state = self.state.lock();
            state.insert(key.clone(), size)
        };
        if !evicted.is_empty() {
            let root = self.root.clone();
            tokio::task::spawn_blocking(move || {
                for victim in evicted {
                    let Ok(victim_path) = validate_artifact_key(&root, &victim) else {
                        continue;
                    };
                    // best-effort eviction; missing file is fine.
                    if let Err(e) = std::fs::remove_file(&victim_path)
                        && e.kind() != std::io::ErrorKind::NotFound
                    {
                        tracing::warn!(path = %victim_path.display(), error = %e, "fs cache: evict failed");
                    }
                }
            })
            .await
            .map_err(|e| StoreError::Backend(format!("evict join: {e}")))?;
        }
        Ok(bytes)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    use bytes::Bytes;
    use mars_store::ObjectStore;
    use tempfile::TempDir;

    use crate::FsStore;

    fn k(s: &str) -> ArtifactKey {
        ArtifactKey::new(s)
    }

    /// origin wrapper that counts upstream `get` invocations.
    struct CountingStore {
        inner: FsStore,
        gets: AtomicUsize,
    }

    impl CountingStore {
        fn new(inner: FsStore) -> Self {
            Self {
                inner,
                gets: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ObjectStore for CountingStore {
        async fn get(&self, key: &ArtifactKey, expected: ContentHash) -> Result<Bytes, StoreError> {
            self.gets.fetch_add(1, Ordering::SeqCst);
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

    /// origin that blocks on a barrier before serving `get`. lets us hold
    /// the leader inside `origin.get` while many waiters queue up.
    struct BarrierStore {
        inner: FsStore,
        gets: AtomicUsize,
        gate: tokio::sync::Notify,
        gate_open: std::sync::atomic::AtomicBool,
    }

    impl BarrierStore {
        fn new(inner: FsStore) -> Self {
            Self {
                inner,
                gets: AtomicUsize::new(0),
                gate: tokio::sync::Notify::new(),
                gate_open: std::sync::atomic::AtomicBool::new(false),
            }
        }
        fn open_gate(&self) {
            self.gate_open.store(true, Ordering::SeqCst);
            self.gate.notify_waiters();
        }
    }

    #[async_trait]
    impl ObjectStore for BarrierStore {
        async fn get(&self, key: &ArtifactKey, expected: ContentHash) -> Result<Bytes, StoreError> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            // wait until the test opens the gate so callers can pile up.
            while !self.gate_open.load(Ordering::SeqCst) {
                self.gate.notified().await;
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

    #[tokio::test]
    async fn roundtrip_on_miss() {
        let store_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let store = CountingStore::new(FsStore::new(store_dir.path()).unwrap());
        let cache = FsCache::new(cache_dir.path(), u64::MAX).unwrap();

        let body = b"hello-cache".to_vec();
        let key = k("a/b.bin");
        let h = store.inner.put(&key, Bytes::from(body.clone())).await.unwrap();

        let first = cache.get_or_fetch(&key, h, &store).await.unwrap();
        assert_eq!(first.as_ref(), body.as_slice());
        assert_eq!(store.gets.load(Ordering::SeqCst), 1);

        let second = cache.get_or_fetch(&key, h, &store).await.unwrap();
        assert_eq!(second.as_ref(), body.as_slice());
        // second call hits local — origin not consulted again.
        assert_eq!(store.gets.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn single_flight_coalesces() {
        let store_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let inner = FsStore::new(store_dir.path()).unwrap();
        let body = vec![7u8; 4096];
        let key = k("c/d.bin");
        let h = inner.put(&key, Bytes::from(body.clone())).await.unwrap();

        let store = Arc::new(BarrierStore::new(inner));
        let cache = Arc::new(FsCache::new(cache_dir.path(), u64::MAX).unwrap());

        let mut handles = Vec::new();
        for _ in 0..16 {
            let store = store.clone();
            let cache = cache.clone();
            let key = key.clone();
            handles.push(tokio::spawn(async move {
                cache.get_or_fetch(&key, h, store.as_ref()).await
            }));
        }

        // give the leader time to register the flight and enter origin.get.
        for _ in 0..50 {
            if store.gets.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        store.open_gate();

        for h in handles {
            let bytes = h.await.unwrap().unwrap();
            assert_eq!(bytes.as_ref(), body.as_slice());
        }
        assert_eq!(
            store.gets.load(Ordering::SeqCst),
            1,
            "16 concurrent calls produced more than one upstream fetch"
        );
    }

    #[tokio::test]
    async fn eviction_respects_size_budget() {
        let store_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let store = FsStore::new(store_dir.path()).unwrap();
        // 4 KiB budget, ten 1 KiB items; expect ≤4 KiB on disk after fill.
        let cache = FsCache::new(cache_dir.path(), 4096).unwrap();

        let payload = vec![9u8; 1024];
        let h = mars_artifact::compute_content_hash(&payload);

        let mut keys = Vec::new();
        for i in 0..10 {
            let key = k(&format!("e/v{i:02}.bin"));
            store.put(&key, Bytes::from(payload.clone())).await.unwrap();
            cache.get_or_fetch(&key, h, &store).await.unwrap();
            keys.push(key);
        }

        let total = walk_total(cache_dir.path());
        assert!(total <= 4096, "on-disk footprint {total} exceeds budget");
        let state_total = cache.state.lock().total_size;
        assert_eq!(state_total, total);

        // oldest entries should have been evicted.
        let oldest = cache_dir.path().canonicalize().unwrap().join("e/v00.bin");
        assert!(!oldest.exists(), "oldest entry was not evicted");
    }

    #[tokio::test]
    async fn mmap_returns_correct_bytes() {
        let store_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let store = FsStore::new(store_dir.path()).unwrap();
        let cache = FsCache::new(cache_dir.path(), u64::MAX).unwrap();

        // distinctive content so we know we're not getting a coincidental zero buffer.
        let body: Vec<u8> = (0..8192u32).map(|n| (n as u8).wrapping_mul(31)).collect();
        let key = k("m/m.bin");
        let h = store.put(&key, Bytes::from(body.clone())).await.unwrap();

        // first call: write through; second: served from mmap.
        cache.get_or_fetch(&key, h, &store).await.unwrap();
        let mmapped = cache.get_or_fetch(&key, h, &store).await.unwrap();
        assert_eq!(mmapped.as_ref(), body.as_slice());
        assert_eq!(mmapped.len(), body.len());
    }

    #[tokio::test]
    async fn restart_loads_existing_entries_and_evicts_to_budget() {
        let store_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let store = FsStore::new(store_dir.path()).unwrap();

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
                tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            }
        }

        let tight = FsCache::new(cache_dir.path(), 2048).unwrap();
        let total: u64 = walk_total(cache_dir.path());
        assert!(
            total <= 2048,
            "expected on-disk footprint <= 2KiB after reopen, got {total}"
        );
        let state_total = tight.state.lock().total_size;
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
        let after_first = cache.state.lock().total_size;
        assert_eq!(after_first, payload.len() as u64);

        std::fs::remove_file(cache_dir.path().canonicalize().unwrap().join("x/y.bin")).unwrap();
        cache.get_or_fetch(&key, h, &store).await.unwrap();
        let after_second = cache.state.lock().total_size;
        assert_eq!(after_second, payload.len() as u64);
    }

    #[test]
    fn fscache_is_not_clone() {
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

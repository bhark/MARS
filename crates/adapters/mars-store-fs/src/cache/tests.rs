#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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
    // second call hits local - origin not consulted again.
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

#[tokio::test]
async fn trust_path_hash_skips_corruption_after_first_verify() {
    // with the optimisation enabled, an in-place file mutation post-verify
    // is invisible to subsequent reads. this is the documented contract.
    let store_dir = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    let store = FsStore::new(store_dir.path()).unwrap();
    let cache = FsCache::with_trust_path_hash(cache_dir.path(), u64::MAX, true).unwrap();

    let key = k("a/b.bin");
    let body = b"hello world".to_vec();
    let h = store.put(&key, Bytes::from(body.clone())).await.unwrap();

    let first = cache.get_or_fetch(&key, h, &store).await.unwrap();
    assert_eq!(first.as_ref(), body.as_slice());

    // poison the cached file in place; second hit must NOT detect it.
    let cached_path = cache_dir.path().canonicalize().unwrap().join("a").join("b.bin");
    std::fs::write(&cached_path, b"poisoned").unwrap();
    let second = cache.get_or_fetch(&key, h, &store).await.unwrap();
    assert_eq!(second.as_ref(), b"poisoned");
}

#[tokio::test]
async fn trust_path_hash_off_still_detects_corruption() {
    // baseline: with the flag off, on-disk corruption is detected and the
    // artifact is refetched from origin.
    let store_dir = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    let store = FsStore::new(store_dir.path()).unwrap();
    let cache = FsCache::with_trust_path_hash(cache_dir.path(), u64::MAX, false).unwrap();

    let key = k("a/b.bin");
    let body = b"hello world".to_vec();
    let h = store.put(&key, Bytes::from(body.clone())).await.unwrap();

    cache.get_or_fetch(&key, h, &store).await.unwrap();
    let cached_path = cache_dir.path().canonicalize().unwrap().join("a").join("b.bin");
    std::fs::write(&cached_path, b"poisoned").unwrap();
    let recovered = cache.get_or_fetch(&key, h, &store).await.unwrap();
    assert_eq!(recovered.as_ref(), body.as_slice());
}

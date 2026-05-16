//! cache integrity under concurrent leaders + simultaneous eviction.
//!
//! the in-module tests at `src/cache.rs` cover the single-flight contract
//! (same key, many waiters, one origin call) and the size-budget contract
//! (tight cap, oldest evicted). they do not cover the production race where
//! many leaders fetch DIFFERENT keys at the same time while the cap is
//! tighter than the working set - which forces eviction to race against
//! concurrent `state.insert` calls. this test pins that interaction.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use mars_artifact::compute_content_hash;
use mars_store::{LocalCache, ObjectStore, StoreError};
use mars_store_fs::{FsCache, FsStore};
use mars_types::{ArtifactKey, ContentHash};
use tempfile::TempDir;
use tokio::task::JoinSet;

const ENTRY_BYTES: usize = 8 * 1024;
const ENTRY_COUNT: usize = 32;
const CAP_BYTES: u64 = 64 * 1024;

struct CountingOrigin {
    inner: FsStore,
    gets: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl ObjectStore for CountingOrigin {
    async fn get(&self, key: &ArtifactKey, expected: ContentHash) -> Result<Bytes, StoreError> {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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

fn walk_total(root: &Path) -> u64 {
    let mut total = 0u64;
    let rd = std::fs::read_dir(root).unwrap();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_leaders_on_distinct_keys_stay_within_size_budget() {
    let origin_dir = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    let inner = FsStore::new(origin_dir.path()).unwrap();
    // seed origin with ENTRY_COUNT distinct artifacts.
    let payload = vec![0xABu8; ENTRY_BYTES];
    let expected_hash = compute_content_hash(&payload);
    let mut keys = Vec::with_capacity(ENTRY_COUNT);
    for i in 0..ENTRY_COUNT {
        let key = ArtifactKey::new(format!("e/{i:02}.bin"));
        let h = inner.put(&key, Bytes::from(payload.clone())).await.unwrap();
        assert_eq!(
            h, expected_hash,
            "deterministic payload should yield deterministic hash"
        );
        keys.push(key);
    }

    let origin = Arc::new(CountingOrigin {
        inner,
        gets: std::sync::atomic::AtomicUsize::new(0),
    });
    // cap allows only ~8 entries at once; the working set is 32. eviction
    // must run concurrently with the next batch of inserts.
    let cache = Arc::new(FsCache::new(cache_dir.path(), CAP_BYTES).unwrap());

    let mut tasks = JoinSet::new();
    for key in &keys {
        let cache = cache.clone();
        let origin = origin.clone();
        let key = key.clone();
        tasks.spawn(async move { cache.get_or_fetch(&key, expected_hash, origin.as_ref()).await });
    }

    let mut ok = 0usize;
    while let Some(res) = tasks.join_next().await {
        let bytes = res.unwrap().unwrap();
        assert_eq!(bytes.len(), ENTRY_BYTES, "every fetch must return full payload");
        ok += 1;
    }
    assert_eq!(ok, ENTRY_COUNT, "every fetch should have completed");

    // every key is distinct; nothing coalesces, so origin sees every fetch.
    let gets = origin.gets.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        gets >= ENTRY_COUNT,
        "expected at least {ENTRY_COUNT} origin gets, observed {gets}"
    );

    // on-disk footprint must stay within the cap even though inserts and
    // evictions raced. drift here would mean the eviction path lost a key.
    let on_disk = walk_total(cache_dir.path());
    assert!(
        on_disk <= CAP_BYTES,
        "on-disk footprint {on_disk} exceeded cap {CAP_BYTES} under concurrent eviction"
    );

    // give any straggler in-flight evictions a moment to settle. with the
    // current implementation `get_or_fetch` does not return until its own
    // post-insert evictions complete, so this is belt-and-braces.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let on_disk_after = walk_total(cache_dir.path());
    assert!(
        on_disk_after <= CAP_BYTES,
        "on-disk footprint {on_disk_after} drifted above cap post-settle"
    );
}

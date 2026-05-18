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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use parking_lot::Mutex;

use hashlink::LinkedHashMap;

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
        let _ = self.lru.to_back(&key);
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
    /// when true, after a key has been BLAKE3-verified once in this process,
    /// subsequent cache hits skip the rehash. paths embed the content hash
    /// (`{hash}.mars`) so a file at the validated path *is* the artifact
    /// with that hash; bit-rot is the only thing the rehash protects against,
    /// and one rehash per (process, key) is enough for that signal.
    trust_path_hash: bool,
    /// keys whose on-disk content has been BLAKE3-verified at least once.
    /// only consulted when `trust_path_hash` is true.
    verified: Mutex<HashSet<ArtifactKey>>,
}

impl FsCache {
    /// Open / create a cache rooted at `root`. Path is canonicalised. Existing
    /// files under `root` are scanned synchronously to seed the in-memory
    /// accounting (size + lru order by mtime, oldest first).
    ///
    /// `max_size_bytes` is the hard size cap; zero disables eviction.
    pub fn new(root: impl Into<PathBuf>, max_size_bytes: u64) -> Result<Self, StoreError> {
        Self::with_trust_path_hash(root, max_size_bytes, false)
    }

    /// Open / create a cache with the `trust_path_hash` optimisation set.
    /// When `true`, cache hits skip the BLAKE3 rehash after the first
    /// successful verification of a key in this process. See the field doc
    /// on `FsCache::trust_path_hash` for the integrity contract.
    pub fn with_trust_path_hash(
        root: impl Into<PathBuf>,
        max_size_bytes: u64,
        trust_path_hash: bool,
    ) -> Result<Self, StoreError> {
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
            trust_path_hash,
            verified: Mutex::new(HashSet::new()),
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
            // already verified in this process? skip the BLAKE3 rehash on hit.
            // a missing file falls through to the miss path naturally.
            let already_verified = self.trust_path_hash && self.verified.lock().contains(key);

            // try local first; treat NotFound and HashMismatch as miss.
            let local = {
                let p = path.clone();
                tokio::task::spawn_blocking(move || -> Result<Option<Bytes>, StoreError> {
                    match read_mmap(&p)? {
                        None => Ok(None),
                        Some(bytes) => {
                            if already_verified || compute_content_hash(&bytes) == expected {
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
                // a previous leader's atomic_write may have completed after the
                // future was cancelled, in which case state.insert never ran
                // and the lru is unaware of this file. fold it in now so the
                // size budget reflects what's actually on disk; otherwise the
                // touch silently no-ops and disk usage drifts above the cap.
                let evicted = {
                    let mut state = self.state.lock();
                    if state.lru.contains_key(key) {
                        state.touch(key.clone());
                        Vec::new()
                    } else {
                        state.insert(key.clone(), bytes.len() as u64)
                    }
                };
                if self.trust_path_hash {
                    // invariant: `verified` only holds keys currently in
                    // state.lru. record this key, drop entries that just got
                    // evicted (their files are about to disappear and a
                    // re-fetch will need to re-verify).
                    let mut verified = self.verified.lock();
                    if !already_verified {
                        verified.insert(key.clone());
                    }
                    for v in &evicted {
                        verified.remove(v);
                    }
                }
                if !evicted.is_empty() {
                    self.evict_files(evicted).await?;
                }
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
        if self.trust_path_hash {
            // origin.get already hashed; record so future cache hits skip rehash.
            let mut verified = self.verified.lock();
            verified.insert(key.clone());
            // clear any evicted entries: their on-disk file is gone, and a
            // future re-fetch must re-verify.
            for v in &evicted {
                verified.remove(v);
            }
        }
        if !evicted.is_empty() {
            self.evict_files(evicted).await?;
        }
        Ok(bytes)
    }

    async fn evict_files(&self, evicted: Vec<ArtifactKey>) -> Result<(), StoreError> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            for victim in evicted {
                let Ok(victim_path) = validate_artifact_key(&root, &victim) else {
                    continue;
                };
                if let Err(e) = std::fs::remove_file(&victim_path)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(path = %victim_path.display(), error = %e, "fs cache: evict failed");
                }
            }
        })
        .await
        .map_err(|e| StoreError::Backend(format!("evict join: {e}")))
    }
}

#[cfg(test)]
mod tests;

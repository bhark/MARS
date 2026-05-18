//! Local disk cache for fetched vector-file blobs.
//!
//! Layout: `<root>/<scheme>/<sha256(uri)>/<sha256(etag)>`. On cache miss
//! the fetcher pulls the body, writes it to a temp file in the same
//! parent, and atomically renames into the final location so concurrent
//! readers never observe a partial file.
//!
//! Size budget + lru eviction: state is held under a sync `parking_lot`
//! mutex and never crosses an `.await`. on construction the existing
//! root is scanned so in-memory accounting starts consistent with what
//! is on disk; lru order is seeded by mtime (oldest first). a single
//! entry larger than the cap is still inserted but will be evicted by
//! the budget loop - a one-shot warn flags the mis-sized cap.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use hashlink::LinkedHashMap;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::error::VectorFileError;

/// In-memory accounting state for the on-disk cache. Held under a sync
/// mutex; methods must not block on async I/O.
#[derive(Debug)]
struct CacheState {
    total_size: u64,
    // 0 disables eviction (matches `None` cap at the config layer).
    max_size: u64,
    lru: LinkedHashMap<EntryKey, u64>,
}

impl CacheState {
    fn touch(&mut self, key: &EntryKey) {
        let _ = self.lru.to_back(key);
    }

    /// inserts or overwrites `key` with `size`, returns the keys whose
    /// files the caller must unlink to bring the cache back under budget.
    fn insert(&mut self, key: EntryKey, size: u64) -> Vec<EntryKey> {
        let prev = self.lru.insert(key, size);
        self.total_size = self.total_size.saturating_sub(prev.unwrap_or(0)).saturating_add(size);
        self.evict()
    }

    fn evict(&mut self) -> Vec<EntryKey> {
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

/// Identifies a cache entry by its on-disk path components. mirrors the
/// three segments produced by [`DiskCache::entry_path`] so an evicted
/// key maps back to a unique file without re-hashing the original uri.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct EntryKey {
    scheme: String,
    uri_digest: String,
    etag_digest: String,
}

impl EntryKey {
    fn new(uri: &str, etag: &str) -> Self {
        Self {
            scheme: scheme_of(uri).unwrap_or("unknown").to_string(),
            uri_digest: sha256_hex(uri.as_bytes()),
            etag_digest: sha256_hex(etag.as_bytes()),
        }
    }

    fn to_path(&self, root: &Path) -> PathBuf {
        root.join(&self.scheme).join(&self.uri_digest).join(&self.etag_digest)
    }
}

/// Disk cache rooted at a service-configured directory. Keyed by
/// `(uri, etag)`. `cap_bytes` is enforced via lru eviction on every
/// successful put; `None` disables the budget.
#[derive(Debug)]
pub struct DiskCache {
    root: PathBuf,
    state: Mutex<CacheState>,
}

impl DiskCache {
    /// Open (or create) the cache root, scan existing entries to seed
    /// the lru, and trim down to budget if the on-disk footprint already
    /// exceeds the cap.
    pub async fn open(root: impl Into<PathBuf>, cap_bytes: Option<u64>) -> Result<Self, VectorFileError> {
        let root = root.into();
        fs::create_dir_all(&root).await.map_err(|e| VectorFileError::Io {
            what: "cache create_dir_all",
            source: e,
        })?;

        let max_size = cap_bytes.unwrap_or(0);
        let (lru, total_size) = scan_existing(&root).await?;
        let state = CacheState {
            total_size,
            max_size,
            lru,
        };
        let cache = Self {
            root,
            state: Mutex::new(state),
        };
        cache.evict_to_budget().await;
        Ok(cache)
    }

    /// Borrow the cache root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Lookup the cached body for `(uri, etag)`. Returns `None` on miss.
    /// On hit, refreshes the entry's lru position.
    pub async fn get(&self, uri: &str, etag: &str) -> Result<Option<Bytes>, VectorFileError> {
        let key = EntryKey::new(uri, etag);
        let path = key.to_path(&self.root);
        match fs::read(&path).await {
            Ok(v) => {
                self.state.lock().touch(&key);
                Ok(Some(Bytes::from(v)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(VectorFileError::Io {
                what: "cache read",
                source: e,
            }),
        }
    }

    /// Write `bytes` under `(uri, etag)` via a temp-file-and-rename so
    /// readers never see a partial write. Updates lru accounting and
    /// unlinks evicted files outside the state lock.
    pub async fn put(&self, uri: &str, etag: &str, bytes: &Bytes) -> Result<(), VectorFileError> {
        let key = EntryKey::new(uri, etag);
        let final_path = key.to_path(&self.root);
        let parent = final_path
            .parent()
            .ok_or_else(|| VectorFileError::Cache("entry path has no parent".into()))?;
        fs::create_dir_all(parent).await.map_err(|e| VectorFileError::Io {
            what: "cache create_dir_all entry",
            source: e,
        })?;

        // already populated by a concurrent writer? fold into accounting
        // (so we don't drift) and return.
        if let Ok(meta) = fs::metadata(&final_path).await {
            let evicted = self.state.lock().insert(key, meta.len());
            self.unlink_evicted(evicted).await;
            return Ok(());
        }

        // random suffix prevents two processes that happen to share a pid
        // (post-restart pid reuse, separate hosts on a shared mount) from
        // colliding on the same tmp name for the same etag.
        let tmp_path = parent.join(format!(".{}.tmp.{:016x}", &key.etag_digest, rand::random::<u64>()));
        {
            let mut f = fs::File::create(&tmp_path).await.map_err(|e| VectorFileError::Io {
                what: "cache tmp create",
                source: e,
            })?;
            f.write_all(bytes).await.map_err(|e| VectorFileError::Io {
                what: "cache tmp write",
                source: e,
            })?;
            f.sync_all().await.map_err(|e| VectorFileError::Io {
                what: "cache tmp sync",
                source: e,
            })?;
        }
        fs::rename(&tmp_path, &final_path)
            .await
            .map_err(|e| VectorFileError::Io {
                what: "cache rename",
                source: e,
            })?;

        let size = bytes.len() as u64;
        let max_size = self.state.lock().max_size;
        if max_size > 0 && size > max_size {
            tracing::warn!(
                entry_bytes = size,
                cap_bytes = max_size,
                uri = uri,
                "vectorfile cache: single entry exceeds configured cap; cap may be undersized",
            );
        }

        let evicted = self.state.lock().insert(key, size);
        self.unlink_evicted(evicted).await;
        Ok(())
    }

    /// Build the entry path for `(uri, etag)`. Pulled apart so tests can
    /// assert the layout.
    pub fn entry_path(&self, uri: &str, etag: &str) -> PathBuf {
        EntryKey::new(uri, etag).to_path(&self.root)
    }

    async fn evict_to_budget(&self) {
        let evicted = self.state.lock().evict();
        self.unlink_evicted(evicted).await;
    }

    async fn unlink_evicted(&self, evicted: Vec<EntryKey>) {
        for key in evicted {
            let path = key.to_path(&self.root);
            match fs::remove_file(&path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "vectorfile cache: evict failed");
                }
            }
        }
    }
}

fn scheme_of(uri: &str) -> Option<&str> {
    uri.split_once("://").map(|(s, _)| s)
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode_lower(h.finalize())
}

/// walk `root` recursively, collecting `(key, size, mtime)` for every
/// file whose path matches the three-segment layout. tmp files and
/// non-conforming entries are skipped. result is sorted by mtime
/// ascending so the lru is seeded oldest-first.
async fn scan_existing(root: &Path) -> Result<(LinkedHashMap<EntryKey, u64>, u64), VectorFileError> {
    let mut entries: Vec<(EntryKey, u64, SystemTime)> = Vec::new();
    let mut scheme_rd = fs::read_dir(root).await.map_err(|e| VectorFileError::Io {
        what: "cache scan root",
        source: e,
    })?;
    while let Some(scheme_ent) = scheme_rd.next_entry().await.map_err(|e| VectorFileError::Io {
        what: "cache scan root entry",
        source: e,
    })? {
        let scheme_meta = scheme_ent.metadata().await.map_err(|e| VectorFileError::Io {
            what: "cache scan scheme meta",
            source: e,
        })?;
        if !scheme_meta.is_dir() {
            continue;
        }
        let Ok(scheme) = scheme_ent.file_name().into_string() else {
            continue;
        };
        scan_scheme(&scheme_ent.path(), &scheme, &mut entries).await?;
    }
    entries.sort_by_key(|(_, _, mtime)| *mtime);

    let mut lru = LinkedHashMap::new();
    let mut total: u64 = 0;
    for (key, size, _) in entries {
        total = total.saturating_add(size);
        lru.insert(key, size);
    }
    Ok((lru, total))
}

async fn scan_scheme(
    scheme_dir: &Path,
    scheme: &str,
    out: &mut Vec<(EntryKey, u64, SystemTime)>,
) -> Result<(), VectorFileError> {
    let mut uri_rd = fs::read_dir(scheme_dir).await.map_err(|e| VectorFileError::Io {
        what: "cache scan scheme",
        source: e,
    })?;
    while let Some(uri_ent) = uri_rd.next_entry().await.map_err(|e| VectorFileError::Io {
        what: "cache scan scheme entry",
        source: e,
    })? {
        let uri_meta = uri_ent.metadata().await.map_err(|e| VectorFileError::Io {
            what: "cache scan uri meta",
            source: e,
        })?;
        if !uri_meta.is_dir() {
            continue;
        }
        let Ok(uri_digest) = uri_ent.file_name().into_string() else {
            continue;
        };
        let mut etag_rd = fs::read_dir(uri_ent.path()).await.map_err(|e| VectorFileError::Io {
            what: "cache scan uri",
            source: e,
        })?;
        while let Some(etag_ent) = etag_rd.next_entry().await.map_err(|e| VectorFileError::Io {
            what: "cache scan uri entry",
            source: e,
        })? {
            let etag_meta = etag_ent.metadata().await.map_err(|e| VectorFileError::Io {
                what: "cache scan etag meta",
                source: e,
            })?;
            if !etag_meta.is_file() {
                continue;
            }
            let Ok(etag_digest) = etag_ent.file_name().into_string() else {
                continue;
            };
            // skip stale tmp files left by aborted writes
            if etag_digest.starts_with('.') || etag_digest.contains(".tmp.") {
                continue;
            }
            let mtime = etag_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            out.push((
                EntryKey {
                    scheme: scheme.to_string(),
                    uri_digest: uri_digest.clone(),
                    etag_digest,
                },
                etag_meta.len(),
                mtime,
            ));
        }
    }
    Ok(())
}

/// tiny in-crate hex shim - keeps the workspace dep set lean for one
/// hot-path use case. not exported.
mod hex {
    pub(super) fn encode_lower(b: impl AsRef<[u8]>) -> String {
        const ALPH: &[u8; 16] = b"0123456789abcdef";
        let b = b.as_ref();
        let mut out = String::with_capacity(b.len() * 2);
        for &v in b {
            out.push(ALPH[(v >> 4) as usize] as char);
            out.push(ALPH[(v & 0x0f) as usize] as char);
        }
        out
    }
}

impl DiskCache {
    /// Wrap in an `Arc`. Useful in adapter composition.
    #[must_use]
    pub fn arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}

#[cfg(test)]
mod tests;

//! Local disk cache for fetched vector-file blobs.
//!
//! Layout: `<root>/<scheme>/<sha256(uri)>/<etag>`. On cache miss the
//! fetcher pulls the body, writes it to a temp file in the same parent,
//! and atomically renames into the final location so concurrent readers
//! never observe a partial file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::error::VectorFileError;

/// Disk cache rooted at a service-configured directory. Keyed by
/// `(uri, etag)`; `cache_max_size` is honoured on a best-effort basis -
/// when the cache exceeds the cap the adapter logs a warning and proceeds.
#[derive(Debug)]
pub struct DiskCache {
    root: PathBuf,
    cap_bytes: Option<u64>,
    // serialises evictions and concurrent writes for the same key. mpsc /
    // RwLock would let us scale, but a snapshot bootstrap reads one body
    // at a time per binding so the contention is bounded.
    write_guard: Mutex<()>,
}

impl DiskCache {
    /// Open (or create) the cache root.
    pub async fn open(root: impl Into<PathBuf>, cap_bytes: Option<u64>) -> Result<Self, VectorFileError> {
        let root = root.into();
        fs::create_dir_all(&root).await.map_err(|e| VectorFileError::Io {
            what: "cache create_dir_all",
            source: e,
        })?;
        Ok(Self {
            root,
            cap_bytes,
            write_guard: Mutex::new(()),
        })
    }

    /// Borrow the cache root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Lookup the cached body for `(uri, etag)`. Returns `None` on miss.
    pub async fn get(&self, uri: &str, etag: &str) -> Result<Option<Bytes>, VectorFileError> {
        let path = self.entry_path(uri, etag);
        match fs::read(&path).await {
            Ok(v) => Ok(Some(Bytes::from(v))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(VectorFileError::Io {
                what: "cache read",
                source: e,
            }),
        }
    }

    /// Write `bytes` under `(uri, etag)` via a temp-file-and-rename so
    /// readers never see a partial write.
    pub async fn put(&self, uri: &str, etag: &str, bytes: &Bytes) -> Result<(), VectorFileError> {
        let _guard = self.write_guard.lock().await;
        let final_path = self.entry_path(uri, etag);
        let parent = final_path
            .parent()
            .ok_or_else(|| VectorFileError::Cache("entry path has no parent".into()))?;
        fs::create_dir_all(parent).await.map_err(|e| VectorFileError::Io {
            what: "cache create_dir_all entry",
            source: e,
        })?;

        // already populated by a concurrent writer? short-circuit.
        if fs::metadata(&final_path).await.is_ok() {
            return Ok(());
        }

        let tmp_path = parent.join(format!(".{etag}.tmp.{}", std::process::id()));
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

        if let Some(cap) = self.cap_bytes {
            self.warn_if_over_cap(cap).await;
        }
        Ok(())
    }

    /// Build the entry path for `(uri, etag)`. Pulled apart so tests can
    /// assert the layout.
    pub fn entry_path(&self, uri: &str, etag: &str) -> PathBuf {
        let scheme = scheme_of(uri).unwrap_or("unknown");
        let mut h = Sha256::new();
        h.update(uri.as_bytes());
        let digest = hex::encode_lower(h.finalize());
        // path-safe etag: sha digest keeps it ascii and bounded.
        let etag_safe = sanitize_etag(etag);
        self.root.join(scheme).join(digest).join(etag_safe)
    }

    async fn warn_if_over_cap(&self, cap: u64) {
        let used = match dir_size(&self.root).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = ?e, "vectorfile cache size probe failed");
                return;
            }
        };
        if used > cap {
            tracing::warn!(
                used_bytes = used,
                cap_bytes = cap,
                "vectorfile cache exceeded configured cap; eviction not yet implemented",
            );
        }
    }
}

fn scheme_of(uri: &str) -> Option<&str> {
    uri.split_once("://").map(|(s, _)| s)
}

fn sanitize_etag(etag: &str) -> String {
    // hash the etag into hex to avoid path-separator characters and
    // weak-etag quotes (W/"abc") tripping the filesystem.
    let mut h = Sha256::new();
    h.update(etag.as_bytes());
    hex::encode_lower(h.finalize())
}

async fn dir_size(p: &Path) -> Result<u64, std::io::Error> {
    let mut stack = vec![p.to_path_buf()];
    let mut total: u64 = 0;
    while let Some(dir) = stack.pop() {
        let mut rd = match fs::read_dir(&dir).await {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        while let Some(entry) = rd.next_entry().await? {
            let meta = entry.metadata().await?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Ok(total)
}

/// Tiny in-crate hex shim - keeps the workspace dep set lean for one
/// hot-path use case. Not exported.
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

// internal Arc constructor; mostly for compose-time clarity.
impl DiskCache {
    /// Wrap in an `Arc`. Useful in adapter composition.
    #[must_use]
    pub fn arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_writes_then_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = DiskCache::open(tmp.path(), None).await.unwrap();
        let uri = "s3://bucket/x.fgb";
        let etag = "\"abc123\"";
        assert!(cache.get(uri, etag).await.unwrap().is_none());
        let payload = Bytes::from_static(b"hello vector world");
        cache.put(uri, etag, &payload).await.unwrap();
        let got = cache.get(uri, etag).await.unwrap().unwrap();
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn layout_keyed_by_scheme_and_uri() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = DiskCache::open(tmp.path(), None).await.unwrap();
        let p1 = cache.entry_path("s3://bucket/a.fgb", "etag1");
        let p2 = cache.entry_path("file:///tmp/a.fgb", "etag1");
        // different schemes -> different prefixes
        assert!(p1.starts_with(tmp.path().join("s3")));
        assert!(p2.starts_with(tmp.path().join("file")));
        // different uris -> different uri hash
        assert_ne!(p1.parent().unwrap(), p2.parent().unwrap());
    }
}

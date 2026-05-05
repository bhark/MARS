//! filesystem-backed [`ObjectStore`].
//!
//! atomic writes via temp-file-in-same-directory + fsync + rename. all blocking
//! i/o is offloaded to `spawn_blocking` so the async trait surface stays clean.

use std::io::Write;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use bytes::Bytes;
use mars_artifact::compute_content_hash;
use mars_store::{ObjectStore, StoreError};
use mars_types::{ArtifactKey, ContentHash};
use rand::{Rng, distributions::Alphanumeric, thread_rng};

use crate::key::{validate_artifact_key, validate_key};

/// Filesystem `ObjectStore` rooted at a single canonical directory.
#[derive(Debug, Clone)]
pub struct FsStore {
    root: PathBuf,
}

impl FsStore {
    /// Open / create a store at `root`. The path is canonicalised.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let raw = root.into();
        if !raw.exists() {
            std::fs::create_dir_all(&raw).map_err(|e| StoreError::Backend(format!("create root: {e}")))?;
        }
        let root = raw
            .canonicalize()
            .map_err(|e| StoreError::Backend(format!("canonicalise root: {e}")))?;
        if !root.is_dir() {
            return Err(StoreError::Backend(format!(
                "root {} is not a directory",
                root.display()
            )));
        }
        cleanup_tmp_files(&root)?;
        Ok(Self { root })
    }

    /// Canonical, absolute root path.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[async_trait]
impl ObjectStore for FsStore {
    async fn get(&self, key: &ArtifactKey, expected: ContentHash) -> Result<Bytes, StoreError> {
        let path = validate_artifact_key(&self.root, key)?;
        let key_owned = key.clone();
        tokio::task::spawn_blocking(move || -> Result<Bytes, StoreError> {
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(StoreError::NotFound(key_owned));
                }
                Err(e) => return Err(StoreError::Backend(format!("read: {e}"))),
            };
            let actual = compute_content_hash(&bytes);
            if actual != expected {
                return Err(StoreError::HashMismatch { key: key_owned });
            }
            Ok(Bytes::from(bytes))
        })
        .await
        .map_err(|e| StoreError::Backend(format!("join: {e}")))?
    }

    async fn put(&self, key: &ArtifactKey, body: Bytes) -> Result<ContentHash, StoreError> {
        let path = validate_artifact_key(&self.root, key)?;
        tokio::task::spawn_blocking(move || -> Result<ContentHash, StoreError> {
            let hash = compute_content_hash(&body);
            atomic_write(&path, &body)?;
            Ok(hash)
        })
        .await
        .map_err(|e| StoreError::Backend(format!("join: {e}")))?
    }

    async fn delete(&self, key: &ArtifactKey) -> Result<(), StoreError> {
        let path = validate_artifact_key(&self.root, key)?;
        let key_owned = key.clone();
        tokio::task::spawn_blocking(move || -> Result<(), StoreError> {
            match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StoreError::NotFound(key_owned)),
                Err(e) => Err(StoreError::Backend(format!("delete: {e}"))),
            }
        })
        .await
        .map_err(|e| StoreError::Backend(format!("join: {e}")))?
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ArtifactKey>, StoreError> {
        let base = if prefix.is_empty() {
            self.root.clone()
        } else {
            validate_key(&self.root, prefix)?
        };
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<ArtifactKey>, StoreError> {
            let mut out: Vec<String> = Vec::new();
            if !base.exists() {
                return Ok(Vec::new());
            }
            walk(&base, &root, &mut out)?;
            out.sort();
            Ok(out.into_iter().map(ArtifactKey::new).collect())
        })
        .await
        .map_err(|e| StoreError::Backend(format!("join: {e}")))?
    }
}

fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) -> Result<(), StoreError> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| StoreError::Backend(format!("read_dir {}: {e}", dir.display())))?;
    for ent in entries {
        let ent = ent.map_err(|e| StoreError::Backend(format!("readdir: {e}")))?;
        let ft = ent
            .file_type()
            .map_err(|e| StoreError::Backend(format!("file_type: {e}")))?;
        let p = ent.path();
        if ft.is_dir() {
            walk(&p, root, out)?;
        } else if ft.is_file() {
            // skip stale temp files
            if let Some(name) = p.file_name().and_then(|s| s.to_str())
                && name.contains(".tmp.")
            {
                continue;
            }
            let rel = p
                .strip_prefix(root)
                .map_err(|e| StoreError::Backend(format!("strip_prefix: {e}")))?;
            let key = rel
                .to_str()
                .ok_or_else(|| StoreError::Backend("non-utf8 path".into()))?
                .replace('\\', "/");
            out.push(key);
        }
    }
    Ok(())
}

/// walk `root` recursively and delete any files whose name contains `.tmp.`.
/// deletion errors are silently ignored (best-effort cleanup after crashes).
pub(crate) fn cleanup_tmp_files(root: &Path) -> Result<(), StoreError> {
    fn walk(dir: &Path) -> Result<(), StoreError> {
        let entries =
            std::fs::read_dir(dir).map_err(|e| StoreError::Backend(format!("read_dir {}: {e}", dir.display())))?;
        for ent in entries {
            let ent = ent.map_err(|e| StoreError::Backend(format!("readdir: {e}")))?;
            let ft = ent
                .file_type()
                .map_err(|e| StoreError::Backend(format!("file_type: {e}")))?;
            let p = ent.path();
            if ft.is_dir() {
                walk(&p)?;
            } else if ft.is_file()
                && let Some(name) = p.file_name().and_then(|s| s.to_str())
                && name.contains(".tmp.")
            {
                let _ = std::fs::remove_file(&p);
            }
        }
        Ok(())
    }
    walk(root)
}

/// Atomic write: tmp file in same dir, fsync, rename. parents are created.
pub(crate) fn atomic_write(path: &Path, body: &[u8]) -> Result<(), StoreError> {
    let parent = path
        .parent()
        .ok_or_else(|| StoreError::Backend("path has no parent".into()))?;
    std::fs::create_dir_all(parent).map_err(|e| StoreError::Backend(format!("mkdir {}: {e}", parent.display())))?;

    let suffix: String = thread_rng()
        .sample_iter(&Alphanumeric)
        .take(12)
        .map(char::from)
        .collect();
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| StoreError::Backend("bad file name".into()))?;
    let tmp = parent.join(format!("{file_name}.tmp.{suffix}"));

    {
        let mut f = std::fs::File::create(&tmp).map_err(|e| StoreError::Backend(format!("create tmp: {e}")))?;
        f.write_all(body)
            .map_err(|e| StoreError::Backend(format!("write: {e}")))?;
        f.sync_all().map_err(|e| StoreError::Backend(format!("fsync: {e}")))?;
    }

    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        StoreError::Backend(format!("rename: {e}"))
    })?;

    // best-effort fsync of the directory entry
    if let Ok(d) = std::fs::File::open(parent) {
        let _ = d.sync_all();
    }
    Ok(())
}

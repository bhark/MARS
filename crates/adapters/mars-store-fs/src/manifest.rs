//! atomic manifest publisher (SPEC §8.5).
//!
//! writes `manifests/v{N}.json` first, then swaps `manifests/current` to point
//! at it. each write is a temp-file-in-same-directory + fsync + rename. if we
//! die between the body write and the pointer swap, `current` still references
//! the previous version - never a partial.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use mars_store::{ManifestPublisher, StoreError};
use mars_types::Manifest;

use crate::store::atomic_write;

const MANIFEST_DIR: &str = "manifests";
const CURRENT_FILE: &str = "current";

/// Filesystem manifest publisher. Shares its root with [`crate::FsStore`].
#[derive(Debug, Clone)]
pub struct FsPublisher {
    root: PathBuf,
}

impl FsPublisher {
    /// Open / create a publisher rooted at `root`. The `manifests/`
    /// subdirectory is created eagerly.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let raw = root.into();
        if !raw.exists() {
            std::fs::create_dir_all(&raw)
                .map_err(|e| StoreError::Backend(format!("create root: {e}")))?;
        }
        let root = raw
            .canonicalize()
            .map_err(|e| StoreError::Backend(format!("canonicalise root: {e}")))?;
        std::fs::create_dir_all(root.join(MANIFEST_DIR))
            .map_err(|e| StoreError::Backend(format!("create manifests dir: {e}")))?;
        Ok(Self { root })
    }

    /// Reads the body of `manifests/current` (typically `v{N}`). Returns
    /// `None` when no manifest has been published yet.
    pub fn read_current(&self) -> Result<Option<String>, StoreError> {
        let p = self.root.join(MANIFEST_DIR).join(CURRENT_FILE);
        match std::fs::read_to_string(&p) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::Backend(format!("read current: {e}"))),
        }
    }

    /// Path to the manifests directory.
    #[must_use]
    pub fn manifests_dir(&self) -> PathBuf {
        self.root.join(MANIFEST_DIR)
    }

    /// Root path.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[async_trait]
impl ManifestPublisher for FsPublisher {
    async fn publish(&self, manifest: &Manifest) -> Result<u64, StoreError> {
        let n = manifest.version;
        let body = serde_json::to_vec_pretty(manifest)
            .map_err(|e| StoreError::Backend(format!("serialise manifest: {e}")))?;
        let dir = self.manifests_dir();
        tokio::task::spawn_blocking(move || -> Result<u64, StoreError> {
            std::fs::create_dir_all(&dir)
                .map_err(|e| StoreError::Backend(format!("mkdir manifests: {e}")))?;
            let body_path = dir.join(format!("v{n}.json"));
            atomic_write(&body_path, &body)?;
            let cur_path = dir.join(CURRENT_FILE);
            atomic_write(&cur_path, format!("v{n}").as_bytes())?;
            Ok(n)
        })
        .await
        .map_err(|e| StoreError::Backend(format!("join: {e}")))?
    }
}

//! atomic manifest publisher (SPEC §8.5).
//!
//! writes `manifests/v{N}.json` first, then swaps `manifests/current` to point
//! at it. each write is a temp-file-in-same-directory + fsync + rename. if we
//! die between the body write and the pointer swap, `current` still references
//! the previous version - never a partial.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use futures_util::stream;
use mars_store::{ManifestPublisher, ManifestReader, ManifestWatch, StoreError};
use mars_types::Manifest;

use crate::store::atomic_write;

const MANIFEST_DIR: &str = "manifests";
const CURRENT_FILE: &str = "current";
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Filesystem manifest publisher. Shares its root with [`crate::FsStore`].
#[derive(Debug, Clone)]
pub struct FsPublisher {
    root: PathBuf,
    poll_interval: Duration,
}

impl FsPublisher {
    /// Open / create a publisher rooted at `root`. The `manifests/`
    /// subdirectory is created eagerly.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let raw = root.into();
        if !raw.exists() {
            std::fs::create_dir_all(&raw).map_err(|e| StoreError::Backend(format!("create root: {e}")))?;
        }
        let root = raw
            .canonicalize()
            .map_err(|e| StoreError::Backend(format!("canonicalise root: {e}")))?;
        std::fs::create_dir_all(root.join(MANIFEST_DIR))
            .map_err(|e| StoreError::Backend(format!("create manifests dir: {e}")))?;
        Ok(Self {
            root,
            poll_interval: DEFAULT_POLL_INTERVAL,
        })
    }

    /// Open / create a publisher with an explicit manifest-watch poll interval.
    pub fn new_with_poll_interval(root: impl Into<PathBuf>, poll_interval: Duration) -> Result<Self, StoreError> {
        let mut publisher = Self::new(root)?;
        publisher.poll_interval = poll_interval;
        Ok(publisher)
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

    async fn read_current_manifest(root: PathBuf) -> Result<Option<(String, Manifest)>, StoreError> {
        let current_path = root.join(MANIFEST_DIR).join(CURRENT_FILE);
        let pointer = match tokio::fs::read_to_string(&current_path).await {
            Ok(pointer) => pointer.trim().to_owned(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(StoreError::Backend(format!("read current: {e}"))),
        };
        if pointer.is_empty() || pointer.contains('/') || pointer.contains('\\') || pointer.contains("..") {
            return Err(StoreError::Backend(format!("malformed manifest pointer: {pointer:?}")));
        }

        let body_path = root.join(MANIFEST_DIR).join(format!("{pointer}.json"));
        let body = tokio::fs::read(&body_path)
            .await
            .map_err(|e| StoreError::Backend(format!("read manifest {pointer}: {e}")))?;
        let manifest =
            serde_json::from_slice(&body).map_err(|e| StoreError::Backend(format!("parse manifest {pointer}: {e}")))?;
        Ok(Some((pointer, manifest)))
    }
}

#[async_trait]
impl ManifestPublisher for FsPublisher {
    async fn publish(&self, manifest: &Manifest) -> Result<u64, StoreError> {
        let n = manifest.version;
        let body =
            serde_json::to_vec_pretty(manifest).map_err(|e| StoreError::Backend(format!("serialise manifest: {e}")))?;
        let dir = self.manifests_dir();
        tokio::task::spawn_blocking(move || -> Result<u64, StoreError> {
            std::fs::create_dir_all(&dir).map_err(|e| StoreError::Backend(format!("mkdir manifests: {e}")))?;
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

#[async_trait]
impl ManifestReader for FsPublisher {
    async fn current_manifest(&self) -> Result<Option<Manifest>, StoreError> {
        match Self::read_current_manifest(self.root.clone()).await? {
            Some((_pointer, manifest)) => Ok(Some(manifest)),
            None => Ok(None),
        }
    }
}

#[async_trait]
impl ManifestWatch for FsPublisher {
    async fn watch(&self) -> Result<BoxStream<'static, Result<Manifest, StoreError>>, StoreError> {
        #[derive(Debug)]
        struct WatchState {
            root: PathBuf,
            poll_interval: Duration,
            last_pointer: Option<String>,
            sleep_before_read: bool,
        }

        let state = WatchState {
            root: self.root.clone(),
            poll_interval: self.poll_interval,
            last_pointer: None,
            sleep_before_read: false,
        };
        let stream = stream::unfold(state, |mut state| async move {
            loop {
                if state.sleep_before_read {
                    tokio::time::sleep(state.poll_interval).await;
                    state.sleep_before_read = false;
                }

                match Self::read_current_manifest(state.root.clone()).await {
                    Ok(Some((pointer, manifest))) if state.last_pointer.as_deref() != Some(pointer.as_str()) => {
                        state.last_pointer = Some(pointer);
                        return Some((Ok(manifest), state));
                    }
                    Ok(Some(_)) | Ok(None) => {}
                    Err(e) => {
                        state.sleep_before_read = true;
                        return Some((Err(e), state));
                    }
                }
                tokio::time::sleep(state.poll_interval).await;
            }
        });
        Ok(Box::pin(stream))
    }
}

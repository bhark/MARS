//! atomic manifest publisher.
//!
//! writes `manifests/v{N}.json` first, then swaps `manifests/current` to point
//! at it. each write is a temp-file-in-same-directory + fsync + rename. if the
//! process dies between the body write and the pointer swap, `current` still references
//! the previous version - never a partial.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use futures_util::stream;
use mars_store::{ManifestStore, StoreError};
use mars_types::Manifest;

use crate::store::{CreateNewOutcome, atomic_create_new, atomic_write};

const MANIFEST_DIR: &str = "manifests";
const CURRENT_FILE: &str = "current";

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
            poll_interval: crate::DEFAULT_POLL_INTERVAL,
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

    async fn read_current_manifest(root: PathBuf) -> Result<Option<(String, SystemTime, Manifest)>, StoreError> {
        let current_path = root.join(MANIFEST_DIR).join(CURRENT_FILE);
        // capture mtime alongside pointer so the watcher can distinguish
        // pointer="vN" → "vM" → "vN" (a republish-and-rollback within one
        // poll interval) from a steady "vN" - requires emitting
        // on every pointer change.
        let mtime = match tokio::fs::metadata(&current_path).await {
            Ok(m) => m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(StoreError::Backend(format!("stat current: {e}"))),
        };
        let pointer = match tokio::fs::read_to_string(&current_path).await {
            Ok(pointer) => pointer.trim().to_owned(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(StoreError::Backend(format!("read current: {e}"))),
        };
        if let Err(e) = mars_types::validate_manifest_pointer(&pointer) {
            return Err(StoreError::Backend(format!(
                "malformed manifest pointer {pointer:?}: {e}"
            )));
        }

        let body_path = root.join(MANIFEST_DIR).join(format!("{pointer}.json"));
        let body = tokio::fs::read(&body_path)
            .await
            .map_err(|e| StoreError::Backend(format!("read manifest {pointer}: {e}")))?;
        let manifest = mars_store::decode_manifest(&body, &pointer)?;
        Ok(Some((pointer, mtime, manifest)))
    }
}

#[async_trait]
impl ManifestStore for FsPublisher {
    async fn publish(&self, manifest: &Manifest) -> Result<u64, StoreError> {
        let n = manifest.version;
        let body =
            serde_json::to_vec_pretty(manifest).map_err(|e| StoreError::Backend(format!("serialise manifest: {e}")))?;
        let dir = self.manifests_dir();
        tokio::task::spawn_blocking(move || -> Result<u64, StoreError> {
            std::fs::create_dir_all(&dir).map_err(|e| StoreError::Backend(format!("mkdir manifests: {e}")))?;
            let body_path = dir.join(format!("v{n}.json"));
            // create-only mirrors S3's PutMode::Create: a duplicate version
            // signals an orphaned publish (crash between body write and
            // pointer swap) and must not be silently overwritten.
            match atomic_create_new(&body_path, &body)? {
                CreateNewOutcome::Created => {}
                CreateNewOutcome::AlreadyExists => {
                    return Err(StoreError::Backend(format!(
                        "manifest body v{n} already exists; refusing to overwrite (orphaned publish or concurrent writer)"
                    )));
                }
            }
            let cur_path = dir.join(CURRENT_FILE);
            atomic_write(&cur_path, format!("v{n}").as_bytes())?;
            Ok(n)
        })
        .await
        .map_err(|e| StoreError::Backend(format!("join: {e}")))?
    }

    async fn current(&self) -> Result<Option<Manifest>, StoreError> {
        match Self::read_current_manifest(self.root.clone()).await? {
            Some((_pointer, _mtime, manifest)) => Ok(Some(manifest)),
            None => Ok(None),
        }
    }

    async fn watch(&self) -> Result<BoxStream<'static, Result<Manifest, StoreError>>, StoreError> {
        #[derive(Debug)]
        struct WatchState {
            root: PathBuf,
            poll_interval: Duration,
            last_pointer: Option<String>,
            last_mtime: Option<SystemTime>,
            sleep_before_read: bool,
        }

        let state = WatchState {
            root: self.root.clone(),
            poll_interval: self.poll_interval,
            last_pointer: None,
            last_mtime: None,
            sleep_before_read: false,
        };
        let stream = stream::unfold(state, |mut state| async move {
            loop {
                if state.sleep_before_read {
                    tokio::time::sleep(state.poll_interval).await;
                    state.sleep_before_read = false;
                }

                match Self::read_current_manifest(state.root.clone()).await {
                    Ok(Some((pointer, mtime, manifest)))
                        if state.last_pointer.as_deref() != Some(pointer.as_str())
                            || state.last_mtime != Some(mtime) =>
                    {
                        state.last_pointer = Some(pointer);
                        state.last_mtime = Some(mtime);
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

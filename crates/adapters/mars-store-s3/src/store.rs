//! `ObjectStore` implementation backed by the `object_store` crate.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use mars_artifact::compute_content_hash;
use mars_store::{ObjectStore, StoreError};
use mars_types::{ArtifactKey, ContentHash};
use object_store::path::Path as OsPath;
use object_store::{ObjectStore as OsStore, ObjectStoreExt};

use crate::config::S3Config;

/// Object-store-backed adapter. Implements `mars_store::ObjectStore`. Holds an
/// `Arc<dyn object_store::ObjectStore>` so test harnesses can substitute
/// `object_store::memory::InMemory` in place of the real S3 client.
#[derive(Clone)]
pub struct S3Store {
    backend: Arc<dyn OsStore>,
    prefix: String,
}

impl std::fmt::Debug for S3Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Store").field("prefix", &self.prefix).finish()
    }
}

impl S3Store {
    /// Build from an `S3Config`, instantiating an `AmazonS3` client.
    pub fn from_config(cfg: &S3Config) -> Result<Self, StoreError> {
        cfg.validate()?;
        let s3 = cfg
            .builder()
            .build()
            .map_err(|e| StoreError::Backend(format!("s3 build: {e}")))?;
        Ok(Self::from_backend(Arc::new(s3), cfg.prefix.clone()))
    }

    /// Wrap an arbitrary `object_store` backend; primarily for tests.
    #[must_use]
    pub fn from_backend(backend: Arc<dyn OsStore>, prefix: String) -> Self {
        Self {
            backend,
            prefix: normalise_prefix(&prefix),
        }
    }

    /// Shared backend handle, for sharing with sibling adapters
    /// (e.g. `S3Publisher`).
    #[must_use]
    pub fn backend(&self) -> Arc<dyn OsStore> {
        self.backend.clone()
    }

    /// Configured prefix (no leading or trailing slash, may be empty).
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub(crate) fn os_path(&self, key: &str) -> OsPath {
        join_prefix(&self.prefix, key)
    }
}

#[async_trait]
impl ObjectStore for S3Store {
    async fn get(&self, key: &ArtifactKey, expected: ContentHash) -> Result<Bytes, StoreError> {
        validate_key(key.as_str())?;
        let path = self.os_path(key.as_str());
        let result = self.backend.get(&path).await.map_err(|e| map_get_error(e, key))?;
        let bytes = result
            .bytes()
            .await
            .map_err(|e| StoreError::Backend(format!("s3 read body: {e}")))?;
        let actual = compute_content_hash(&bytes);
        if actual != expected {
            return Err(StoreError::HashMismatch { key: key.clone() });
        }
        Ok(bytes)
    }

    async fn put(&self, key: &ArtifactKey, body: Bytes) -> Result<ContentHash, StoreError> {
        validate_key(key.as_str())?;
        let path = self.os_path(key.as_str());
        let hash = compute_content_hash(&body);
        self.backend
            .put(&path, body.into())
            .await
            .map_err(|e| StoreError::Backend(format!("s3 put: {e}")))?;
        Ok(hash)
    }

    async fn delete(&self, key: &ArtifactKey) -> Result<(), StoreError> {
        validate_key(key.as_str())?;
        let path = self.os_path(key.as_str());
        match self.backend.delete(&path).await {
            // AWS S3 DeleteObject is idempotent; mirror that here.
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(StoreError::Backend(format!("s3 delete: {e}"))),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ArtifactKey>, StoreError> {
        if !prefix.is_empty() {
            validate_key(prefix)?;
        }
        let full = if prefix.is_empty() {
            // empty prefix => list everything under the configured prefix
            if self.prefix.is_empty() {
                None
            } else {
                Some(OsPath::from(self.prefix.as_str()))
            }
        } else {
            Some(self.os_path(prefix))
        };

        let mut stream = self.backend.list(full.as_ref());
        let mut out: Vec<String> = Vec::new();
        while let Some(item) = stream.next().await {
            let meta = item.map_err(|e| StoreError::Backend(format!("s3 list: {e}")))?;
            let key = strip_prefix(&self.prefix, meta.location.as_ref());
            // skip empties from any quirky backend
            if !key.is_empty() {
                out.push(key);
            }
        }
        out.sort();
        Ok(out.into_iter().map(ArtifactKey::new).collect())
    }
}

fn map_get_error(e: object_store::Error, key: &ArtifactKey) -> StoreError {
    match e {
        object_store::Error::NotFound { .. } => StoreError::NotFound(key.clone()),
        other => StoreError::Backend(format!("s3 get: {other}")),
    }
}

/// reject obvious key abuses. the bucket-side key space is flat, but we still
/// want to keep our keys looking like the fs adapter so the cache layout
/// mirrors. forbidden: empty, leading slash, parent-dir, NUL, backslash.
pub(crate) fn validate_key(key: &str) -> Result<(), StoreError> {
    if key.is_empty() {
        return Err(StoreError::Backend("empty key".into()));
    }
    if key.starts_with('/') {
        return Err(StoreError::Backend("leading slash in key".into()));
    }
    if key.contains('\\') {
        return Err(StoreError::Backend("backslash in key".into()));
    }
    if key.contains('\0') {
        return Err(StoreError::Backend("NUL byte in key".into()));
    }
    for seg in key.split('/') {
        match seg {
            "" => return Err(StoreError::Backend("empty path segment".into())),
            "." | ".." => return Err(StoreError::Backend("relative segment in key".into())),
            _ => {}
        }
    }
    Ok(())
}

fn normalise_prefix(p: &str) -> String {
    p.trim_matches('/').to_owned()
}

pub(crate) fn join_prefix(prefix: &str, key: &str) -> OsPath {
    if prefix.is_empty() {
        OsPath::from(key)
    } else {
        OsPath::from(format!("{prefix}/{key}"))
    }
}

fn strip_prefix(prefix: &str, full: &str) -> String {
    if prefix.is_empty() {
        return full.to_owned();
    }
    let p = format!("{prefix}/");
    full.strip_prefix(&p).unwrap_or(full).to_owned()
}

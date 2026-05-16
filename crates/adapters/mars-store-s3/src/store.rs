//! `ObjectStore` implementation backed by the `object_store` crate.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use mars_artifact::compute_content_hash;
use mars_store::{ObjectStore, StoreError};
use mars_types::{ArtifactKey, ContentHash};
use object_store::path::Path as OsPath;
use object_store::{ObjectStore as OsStore, ObjectStoreExt};

use crate::config::S3Config;

const RETRY_DELAYS: &[Duration] = &[
    Duration::from_millis(50),
    Duration::from_millis(200),
    Duration::from_millis(800),
];

/// Returns true for errors that are worth retrying. Conservatively limited to
/// `Generic` (network / 5xx-class) and `JoinError`. Anything semantic
/// (NotFound, Precondition, AlreadyExists, NotSupported, PermissionDenied,
/// Unauthenticated, UnknownConfigurationKey, InvalidPath, NotImplemented,
/// NotModified) is terminal and must not be retried.
pub(crate) fn is_transient(e: &object_store::Error) -> bool {
    matches!(
        e,
        object_store::Error::Generic { .. } | object_store::Error::JoinError { .. }
    )
}

/// Map an `object_store::Error` to a [`StoreError`], routing transient
/// failures to the dedicated `Transient` variant so callers can retry.
pub(crate) fn map_backend_error(prefix: &str, e: object_store::Error) -> StoreError {
    if is_transient(&e) {
        StoreError::Transient(format!("{prefix}: {e}"))
    } else {
        StoreError::Backend(format!("{prefix}: {e}"))
    }
}

/// Run `f` with capped exponential backoff. Only [`is_transient`] failures are
/// retried; the final attempt's error is propagated as-is.
pub(crate) async fn retry_transient<T, F, Fut>(mut f: F) -> object_store::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = object_store::Result<T>>,
{
    let mut delays = RETRY_DELAYS.iter();
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if is_transient(&e) => match delays.next() {
                Some(d) => {
                    tracing::warn!(error = %e, delay_ms = d.as_millis() as u64, "s3 transient; retrying");
                    tokio::time::sleep(*d).await;
                }
                None => return Err(e),
            },
            Err(e) => return Err(e),
        }
    }
}

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
        let bytes = retry_transient(|| async {
            let result = self.backend.get(&path).await?;
            result.bytes().await
        })
        .await
        .map_err(|e| map_get_error(e, key))?;
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
            .map_err(|e| map_backend_error("s3 put", e))?;
        Ok(hash)
    }

    async fn delete(&self, key: &ArtifactKey) -> Result<(), StoreError> {
        validate_key(key.as_str())?;
        let path = self.os_path(key.as_str());
        match self.backend.delete(&path).await {
            // AWS S3 DeleteObject is idempotent; mirror that here.
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            // garage routes a missing key through the Multi-Object Delete
            // endpoint and surfaces it as Generic("...NoSuchKey..."). normalise
            // so the idempotent contract holds across self-hosted backends.
            Err(object_store::Error::Generic { source, .. }) if source.to_string().contains("NoSuchKey") => Ok(()),
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
        other => map_backend_error("s3 get", other),
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

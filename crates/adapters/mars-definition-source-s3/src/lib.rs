//! S3-compatible object-store adapter for [`mars_definition_source::DefinitionSource`].
//!
//! Resolves a `RenderDefinition` payload from a single object key in an S3 /
//! MinIO / R2 / GCS bucket via the shared `object_store` crate. `watch` polls
//! the object's ETag on the configured interval and emits a [`Change`] only
//! when the ETag differs from the previously observed value (HEAD-only -
//! payload is re-fetched on demand by the reconciler).
//!
//! Credentials: when no explicit triple is supplied the adapter falls back to
//! `object_store`'s default credential chain (env vars, IRSA, instance
//! profile), matching the production posture of `mars-store-s3`. An explicit
//! [`S3Credentials`] bundle short-circuits the chain.

#![deny(unsafe_code)]
#![deny(missing_docs)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use futures_util::StreamExt;
use mars_definition_source::{Change, DefinitionBytes, DefinitionSource, DefinitionSourceError};
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as OsPath;
use object_store::{ObjectStore as OsStore, ObjectStoreExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::warn;

const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

// small channel: watch only emits on ETag delta, so consumer back-pressure at
// worst drops near-simultaneous duplicates.
const WATCH_CHANNEL_CAP: usize = 8;

/// Explicit S3 credentials. When `None` is passed to the adapter, the default
/// credential chain (env vars, IRSA, instance profile) is used instead.
#[derive(Clone)]
pub struct S3Credentials {
    /// AWS access key id (or compatible).
    pub access_key: String,
    /// AWS secret access key (or compatible).
    pub secret_key: String,
    /// Optional STS session token for temporary credentials.
    pub session_token: Option<String>,
}

impl std::fmt::Debug for S3Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // never log credential material; only acknowledge presence
        f.debug_struct("S3Credentials")
            .field("access_key", &"<redacted>")
            .field("secret_key", &"<redacted>")
            .field("session_token", &self.session_token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Adapter that resolves a `RenderDefinition` payload from an S3-compatible bucket.
#[derive(Clone)]
pub struct S3DefinitionSource {
    inner: Arc<Inner>,
}

struct Inner {
    bucket: String,
    key: String,
    interval: Duration,
    backend: Arc<dyn OsStore>,
}

impl S3DefinitionSource {
    /// Build the adapter. `endpoint = None` selects the default AWS endpoint.
    /// `credentials = None` falls back to `object_store`'s default credential
    /// chain. `interval = None` defaults to 60s.
    pub fn new(
        endpoint: Option<String>,
        region: String,
        bucket: String,
        key: String,
        interval: Option<Duration>,
        credentials: Option<S3Credentials>,
    ) -> Result<Self, S3ConfigError> {
        if bucket.trim().is_empty() {
            return Err(S3ConfigError::EmptyBucket);
        }
        if key.trim().is_empty() {
            return Err(S3ConfigError::EmptyKey);
        }
        if region.trim().is_empty() {
            return Err(S3ConfigError::EmptyRegion);
        }
        if let Some(c) = &credentials
            && (c.access_key.is_empty() || c.secret_key.is_empty())
        {
            return Err(S3ConfigError::IncompleteCredentials);
        }

        let mut b = AmazonS3Builder::new().with_bucket_name(&bucket).with_region(&region);
        if let Some(ep) = &endpoint {
            // allow http only when the operator explicitly pointed at one (e.g. MinIO over
            // a cluster-local URL); the default AWS endpoint stays https.
            let lower = ep.to_ascii_lowercase();
            if lower.starts_with("http://") {
                b = b.with_allow_http(true);
            }
            b = b.with_endpoint(ep);
        }
        if let Some(c) = credentials {
            b = b
                .with_access_key_id(&c.access_key)
                .with_secret_access_key(&c.secret_key);
            if let Some(t) = c.session_token {
                b = b.with_token(t);
            }
        }
        let backend = b.build().map_err(|e| S3ConfigError::Build(e.to_string()))?;

        Ok(Self {
            inner: Arc::new(Inner {
                bucket,
                key,
                interval: interval.unwrap_or(DEFAULT_INTERVAL),
                backend: Arc::new(backend),
            }),
        })
    }

    fn target_uri(&self) -> String {
        format!("s3://{}/{}", self.inner.bucket, self.inner.key)
    }
}

#[async_trait]
impl DefinitionSource for S3DefinitionSource {
    async fn fetch(&self) -> Result<DefinitionBytes, DefinitionSourceError> {
        let path = OsPath::from(self.inner.key.as_str());
        let target = self.target_uri();
        let result = self
            .inner
            .backend
            .get(&path)
            .await
            .map_err(|e| map_os_error("s3 get", &target, e))?;
        let etag = result.meta.e_tag.clone();
        let data = result
            .bytes()
            .await
            .map_err(|e| map_os_error("s3 get body", &target, e))?;
        let revision = etag.map(strip_etag_quotes).ok_or(DefinitionSourceError::Decode {
            what: "s3 object missing ETag",
            source: Box::new(MissingEtag),
        })?;
        Ok(DefinitionBytes { data, revision })
    }

    fn watch(&self) -> BoxStream<'static, Change> {
        let (tx, rx) = mpsc::channel::<Change>(WATCH_CHANNEL_CAP);
        let inner = Arc::clone(&self.inner);
        let target = self.target_uri();

        tokio::spawn(async move {
            let mut last_revision: Option<String> = None;
            let mut tick = tokio::time::interval(inner.interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                tick.tick().await;
                let path = OsPath::from(inner.key.as_str());
                let meta = match inner.backend.head(&path).await {
                    Ok(m) => m,
                    Err(e) => {
                        warn!(target: "mars-definition-source-s3", error = %e, uri = %target, "s3 poll: head failed");
                        continue;
                    }
                };
                let rev = match meta.e_tag.map(strip_etag_quotes) {
                    Some(r) => r,
                    None => {
                        warn!(target: "mars-definition-source-s3", uri = %target, "s3 poll: head response missing ETag");
                        continue;
                    }
                };
                if last_revision.as_deref() == Some(rev.as_str()) {
                    continue;
                }
                last_revision = Some(rev.clone());
                if tx.send(Change { revision: rev }).await.is_err() {
                    break;
                }
            }
        });

        ReceiverStream::new(rx).boxed()
    }
}

/// Configuration errors raised at construction.
#[derive(Debug, thiserror::Error)]
pub enum S3ConfigError {
    /// `bucket` was empty or whitespace.
    #[error("s3 bucket must not be empty")]
    EmptyBucket,
    /// `key` was empty or whitespace.
    #[error("s3 key must not be empty")]
    EmptyKey,
    /// `region` was empty or whitespace.
    #[error("s3 region must not be empty")]
    EmptyRegion,
    /// Explicit credentials present but `access_key` or `secret_key` empty.
    #[error("s3 credentials: access_key and secret_key are both required when set")]
    IncompleteCredentials,
    /// `object_store` rejected the builder configuration.
    #[error("s3 builder: {0}")]
    Build(String),
}

impl From<S3ConfigError> for DefinitionSourceError {
    fn from(e: S3ConfigError) -> Self {
        DefinitionSourceError::Other { message: e.to_string() }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("s3 object missing ETag")]
struct MissingEtag;

/// Strip the wrapping `"…"` that S3 returns around ETag values, leaving the
/// inner hex (or W/-prefixed weak validator) intact.
pub(crate) fn strip_etag_quotes(mut s: String) -> String {
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        s.pop();
        s.remove(0);
    }
    s
}

fn map_os_error(what: &'static str, target: &str, e: object_store::Error) -> DefinitionSourceError {
    match &e {
        object_store::Error::NotFound { .. } => DefinitionSourceError::NotFound {
            what: target.to_string(),
        },
        // object_store routes 401/403 through PermissionDenied / Unauthenticated where the
        // backend distinguishes them; otherwise the s3 client surfaces them as Generic with
        // the status in the message. coarse heuristic, same posture as the git adapter.
        object_store::Error::PermissionDenied { .. } | object_store::Error::Unauthenticated { .. } => {
            DefinitionSourceError::Auth {
                what: format!("{target}: {e}"),
            }
        }
        object_store::Error::Generic { source, .. } => {
            let msg = source.to_string().to_ascii_lowercase();
            if msg.contains("403") || msg.contains("401") || msg.contains("forbidden") || msg.contains("unauthorized") {
                DefinitionSourceError::Auth {
                    what: format!("{target}: {e}"),
                }
            } else {
                DefinitionSourceError::network(what, e)
            }
        }
        _ => DefinitionSourceError::network(what, e),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;

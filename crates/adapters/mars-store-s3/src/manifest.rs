//! object-store-backed [`ManifestStore`] (SPEC §8.5).
//!
//! publish flow:
//!   1. write `manifests/v{N}.json` with `PutMode::Create` so a duplicate
//!      version errors instead of being silently overwritten.
//!   2. CAS-update `manifests/current` from its prior etag.
//!
//! a backend that does not support `PutMode::Update` returns
//! `NotSupported`; we fall back to plain overwrite and log a warning. for
//! AWS S3 + MinIO + R2 the update path is supported.
//!
//! singleton-compiler invariant: only one compiler process publishes at a
//! time (enforced by the leader-lock in mars-source). step 1's create-only
//! write is a belt-and-braces guard: if two writers ever race past the
//! leader lock, one body write fails fast rather than silently overwriting
//! the peer's serialised manifest.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::stream;
use mars_store::{ManifestStore, StoreError};
use mars_types::{MANIFEST_FORMAT_VERSION, Manifest};
use object_store::path::Path as OsPath;
use object_store::{GetOptions, ObjectStore as OsStore, ObjectStoreExt, PutMode, PutOptions, UpdateVersion};

use crate::store::{S3Store, join_prefix, map_backend_error, retry_transient};

/// Result of a conditional read of `manifests/current`.
enum ReadCurrent {
    Body {
        pointer: String,
        version: Option<UpdateVersion>,
    },
    NotModified,
    Missing,
}

const MANIFEST_DIR: &str = "manifests";
const CURRENT_FILE: &str = "manifests/current";

/// Steady-state poll cadence on `manifests/current`. Compiler windows are on
/// the order of minutes, so a per-second poll on every replica is wasted GETs
/// against the bucket. 15 s keeps freshness within one or two compile windows
/// without paying ongoing scrape cost; the conditional GET below makes the
/// happy path a no-body 304 when the pointer hasn't moved.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(15);

/// object-store-backed manifest publisher. shares its backend (and prefix)
/// with [`S3Store`] so a single bucket holds artifacts and manifests.
#[derive(Clone)]
pub struct S3Publisher {
    backend: Arc<dyn OsStore>,
    prefix: String,
    poll_interval: Duration,
    allow_non_atomic_publish: bool,
}

impl std::fmt::Debug for S3Publisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Publisher")
            .field("prefix", &self.prefix)
            .field("poll_interval", &self.poll_interval)
            .finish()
    }
}

impl S3Publisher {
    /// Build from an existing `S3Store` so artifacts and manifests share a
    /// backend handle.
    #[must_use]
    pub fn from_store(store: &S3Store) -> Self {
        Self {
            backend: store.backend(),
            prefix: store.prefix().to_owned(),
            poll_interval: DEFAULT_POLL_INTERVAL,
            allow_non_atomic_publish: false,
        }
    }

    /// Override the watch poll interval.
    #[must_use]
    pub fn with_poll_interval(mut self, d: Duration) -> Self {
        self.poll_interval = d;
        self
    }

    /// Allow non-atomic manifest publish when the backend lacks CAS support.
    #[must_use]
    pub fn with_allow_non_atomic_publish(mut self, allow: bool) -> Self {
        self.allow_non_atomic_publish = allow;
        self
    }

    fn current_path(&self) -> OsPath {
        join_prefix(&self.prefix, CURRENT_FILE)
    }

    fn body_path(&self, n: u64) -> OsPath {
        join_prefix(&self.prefix, &format!("{MANIFEST_DIR}/v{n}.json"))
    }

    /// Read the raw `manifests/current` body and its etag; returns `None` if
    /// not yet published.
    async fn read_current(&self) -> Result<Option<(String, Option<UpdateVersion>)>, StoreError> {
        self.read_current_if_changed(None).await.map(|res| match res {
            ReadCurrent::Body { pointer, version } => Some((pointer, version)),
            // explicit `None` etag never produces NotModified; assert by mapping it away.
            ReadCurrent::NotModified | ReadCurrent::Missing => None,
        })
    }

    /// Conditional read of `manifests/current`. When `if_none_match` is `Some`
    /// the request includes the etag and the backend returns `NotModified`
    /// (no body) on a hit, turning steady-state polling into metadata-only
    /// roundtrips.
    async fn read_current_if_changed(&self, if_none_match: Option<String>) -> Result<ReadCurrent, StoreError> {
        let path = self.current_path();
        let result = retry_transient(|| async {
            let opts = GetOptions {
                if_none_match: if_none_match.clone(),
                ..GetOptions::default()
            };
            let r = self.backend.get_opts(&path, opts).await?;
            let etag = r.meta.e_tag.clone();
            let version = r.meta.version.clone();
            let bytes = r.bytes().await?;
            Ok((bytes, etag, version))
        })
        .await;
        match result {
            Ok((bytes, etag, version)) => {
                let pointer = String::from_utf8(bytes.to_vec())
                    .map_err(|e| StoreError::Backend(format!("manifest pointer not utf-8: {e}")))?
                    .trim()
                    .to_owned();
                mars_types::validate_manifest_pointer(&pointer)
                    .map_err(|e| StoreError::Backend(format!("malformed manifest pointer {pointer:?}: {e}")))?;
                Ok(ReadCurrent::Body {
                    pointer,
                    version: Some(UpdateVersion { e_tag: etag, version }),
                })
            }
            Err(object_store::Error::NotFound { .. }) => Ok(ReadCurrent::Missing),
            // 304 Not Modified surfaces as Precondition once the conditional
            // matches; same etag means the pointer is unchanged.
            Err(object_store::Error::NotModified { .. }) => Ok(ReadCurrent::NotModified),
            Err(e) => Err(StoreError::Backend(format!("s3 get current: {e}"))),
        }
    }

    async fn fetch_manifest_body(&self, pointer: &str) -> Result<Manifest, StoreError> {
        if let Err(e) = mars_types::validate_manifest_pointer(pointer) {
            return Err(StoreError::Backend(format!(
                "malformed manifest pointer {pointer:?}: {e}"
            )));
        }
        let path = join_prefix(&self.prefix, &format!("{MANIFEST_DIR}/{pointer}.json"));
        let bytes = retry_transient(|| async {
            let r = self.backend.get(&path).await?;
            r.bytes().await
        })
        .await
        .map_err(|e| StoreError::Backend(format!("s3 get manifest {pointer}: {e}")))?;
        // exact-match version gate: v3 is a clean cut from v1/v2, no
        // tolerance for "accept anything <= max" — see manifest version handling.
        // peek format_version before the full decode so structural drift
        // in older payloads surfaces as UnsupportedManifestVersion rather
        // than as a generic serde error.
        let peek: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| StoreError::Backend(format!("parse manifest {pointer}: {e}")))?;
        let found = peek
            .get("format_version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| StoreError::Backend(format!("manifest {pointer}: missing format_version")))?;
        let found_u32 = u32::try_from(found)
            .map_err(|_| StoreError::Backend(format!("manifest {pointer}: format_version overflows u32")))?;
        if found_u32 != MANIFEST_FORMAT_VERSION {
            return Err(StoreError::UnsupportedManifestVersion {
                found: found_u32,
                supported: MANIFEST_FORMAT_VERSION,
            });
        }
        let manifest: Manifest =
            serde_json::from_value(peek).map_err(|e| StoreError::Backend(format!("parse manifest {pointer}: {e}")))?;
        Ok(manifest)
    }
}

#[async_trait]
impl ManifestStore for S3Publisher {
    async fn publish(&self, manifest: &Manifest) -> Result<u64, StoreError> {
        let n = manifest.version;
        let body = Bytes::from(
            serde_json::to_vec_pretty(manifest).map_err(|e| StoreError::Backend(format!("serialise manifest: {e}")))?,
        );
        let body_path = self.body_path(n);
        match self
            .backend
            .put_opts(&body_path, body.clone().into(), PutOptions::from(PutMode::Create))
            .await
        {
            Ok(_) => {}
            // body at v{N} already exists. under the singleton-compiler invariant
            // this means a prior publish left orphaned bytes (crashed between
            // body write and current-pointer CAS); under a leader-lock failure
            // a concurrent writer beat us. either way we refuse to overwrite.
            Err(object_store::Error::AlreadyExists { .. }) => {
                return Err(StoreError::Backend(format!(
                    "manifest body v{n} already exists; refusing to overwrite (orphaned publish or concurrent writer)"
                )));
            }
            Err(object_store::Error::NotSupported { .. }) if self.allow_non_atomic_publish => {
                self.backend
                    .put(&body_path, body.into())
                    .await
                    .map_err(|e| StoreError::Backend(format!("s3 put manifest body: {e}")))?;
            }
            Err(object_store::Error::NotSupported { .. }) => {
                return Err(StoreError::Backend(
                    "s3 backend does not support conditional create; set allow_non_atomic_publish to override".into(),
                ));
            }
            Err(e) => return Err(map_backend_error("s3 put manifest body", e)),
        }

        let pointer_body = Bytes::from(format!("v{n}"));
        let current_path = self.current_path();

        let prior = self.read_current().await?;
        let opts = match prior {
            Some((_, Some(version))) => PutOptions::from(PutMode::Update(version)),
            _ => PutOptions::from(PutMode::Create),
        };

        match self
            .backend
            .put_opts(&current_path, pointer_body.clone().into(), opts)
            .await
        {
            Ok(_) => Ok(n),
            // bucket lacks CAS support -> fall back to overwrite only when allowed.
            Err(object_store::Error::NotSupported { .. }) => {
                if self.allow_non_atomic_publish {
                    tracing::warn!(
                        "s3 backend does not support conditional put; manifest swap is not atomic across writers"
                    );
                    self.backend
                        .put(&current_path, pointer_body.into())
                        .await
                        .map_err(|e| StoreError::Backend(format!("s3 put current: {e}")))?;
                    Ok(n)
                } else {
                    Err(StoreError::Backend(
                        "s3 backend does not support conditional put; set allow_non_atomic_publish to override".into(),
                    ))
                }
            }
            Err(object_store::Error::Precondition { .. } | object_store::Error::AlreadyExists { .. }) => Err(
                StoreError::Transient("manifest pointer changed concurrently; retry publish".into()),
            ),
            Err(e) => Err(map_backend_error("s3 put current", e)),
        }
    }

    async fn current(&self) -> Result<Option<Manifest>, StoreError> {
        let Some((pointer, _)) = self.read_current().await? else {
            return Ok(None);
        };
        Ok(Some(self.fetch_manifest_body(&pointer).await?))
    }

    async fn watch(&self) -> Result<BoxStream<'static, Result<Manifest, StoreError>>, StoreError> {
        struct State {
            publisher: S3Publisher,
            last_pointer: Option<String>,
            // last seen etag of `manifests/current`; powers If-None-Match so
            // steady-state polls are 304s without bodies.
            last_etag: Option<String>,
            sleep_first: bool,
        }
        let state = State {
            publisher: self.clone(),
            last_pointer: None,
            last_etag: None,
            sleep_first: false,
        };
        let stream = stream::unfold(state, |mut state| async move {
            loop {
                if state.sleep_first {
                    tokio::time::sleep(state.publisher.poll_interval).await;
                    state.sleep_first = false;
                }
                let res = state.publisher.read_current_if_changed(state.last_etag.clone()).await;
                match res {
                    Ok(ReadCurrent::Body { pointer, version }) => {
                        // refresh etag whether or not pointer text changed; a
                        // republish under the same vN keeps the etag stable so
                        // future polls stay cheap.
                        state.last_etag = version.as_ref().and_then(|v| v.e_tag.clone());
                        if state.last_pointer.as_deref() != Some(pointer.as_str()) {
                            match state.publisher.fetch_manifest_body(&pointer).await {
                                Ok(m) => {
                                    state.last_pointer = Some(pointer);
                                    return Some((Ok(m), state));
                                }
                                Err(e) => {
                                    state.sleep_first = true;
                                    return Some((Err(e), state));
                                }
                            }
                        }
                    }
                    Ok(ReadCurrent::NotModified | ReadCurrent::Missing) => {}
                    Err(e) => {
                        state.sleep_first = true;
                        return Some((Err(e), state));
                    }
                }
                tokio::time::sleep(state.publisher.poll_interval).await;
            }
        });
        Ok(Box::pin(stream))
    }
}

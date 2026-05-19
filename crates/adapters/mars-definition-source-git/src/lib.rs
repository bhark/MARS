//! Git adapter for [`mars_definition_source::DefinitionSource`].
//!
//! Resolves a `RenderDefinition` payload from a git repository at a named
//! branch / tag / commit, reading a single file path out of the resolved
//! tree. `watch` polls on the configured interval and emits a [`Change`] only
//! when the resolved commit SHA differs from the previously observed one.
//!
//! Auth modes (mutually exclusive; selection is driven by which keys the
//! operator finds in the resolved `Secret`):
//!   * none (public)
//!   * HTTPS basic (`username` + `password`)
//!   * HTTPS bearer (`bearerToken`) - sent as `Authorization: Bearer <t>`
//!   * SSH (`identity` + `identity.pub` + `known_hosts`)
//!
//! mTLS is additive on any HTTPS transport: optional client cert + key, plus
//! optional custom CA bundle. Materialised into the temp working dir and
//! wired in via `http.sslCert` / `http.sslKey` / `http.sslCAInfo`.
//!
//! Implementation note: this adapter uses `git2` (libgit2 bindings) rather
//! than `gix`. `gix`'s SSH transport currently delegates to the system `ssh`
//! binary and cannot consume an in-memory identity + custom `known_hosts`
//! bundle cleanly, and its mTLS surface is not stable. `git2` covers all four
//! auth modes via `RemoteCallbacks::credentials` + the per-repo HTTP config.

#![deny(unsafe_code)]
#![deny(missing_docs)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use futures_util::StreamExt;
use mars_definition_source::{Change, DefinitionBytes, DefinitionSource, DefinitionSourceError};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::warn;

mod auth;
mod fetch;

pub use auth::{GitAuth, GitReference, TlsBundle};

const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

// small channel: change events are coalesced by the poller (it only sends on
// SHA delta), so a backed-up consumer at worst drops a few duplicates.
const WATCH_CHANNEL_CAP: usize = 8;

/// Adapter that resolves a `RenderDefinition` payload from a git repository.
#[derive(Clone)]
pub struct GitDefinitionSource {
    inner: Arc<Inner>,
}

struct Inner {
    url: String,
    reference: GitReference,
    path: String,
    interval: Duration,
    auth: GitAuth,
    tls: Option<TlsBundle>,
}

impl GitDefinitionSource {
    /// Build the adapter from a resolved git config bundle. `interval` is
    /// clamped to a 1m default when `None`; callers wanting a tighter cadence
    /// must pass an explicit `Duration`.
    pub fn new(
        url: String,
        reference: GitReference,
        path: String,
        interval: Option<Duration>,
        auth: GitAuth,
        tls: Option<TlsBundle>,
    ) -> Result<Self, GitConfigError> {
        if url.trim().is_empty() {
            return Err(GitConfigError::EmptyUrl);
        }
        if path.trim().is_empty() {
            return Err(GitConfigError::EmptyPath);
        }
        reference.validate()?;
        auth.validate(&url)?;
        if let Some(t) = &tls {
            t.validate()?;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                url,
                reference,
                path,
                interval: interval.unwrap_or(DEFAULT_INTERVAL),
                auth,
                tls,
            }),
        })
    }
}

#[async_trait]
impl DefinitionSource for GitDefinitionSource {
    async fn fetch(&self) -> Result<DefinitionBytes, DefinitionSourceError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || fetch::fetch_blocking(&inner))
            .await
            .map_err(|e| DefinitionSourceError::Other {
                message: format!("git fetch task panicked: {e}"),
            })?
    }

    fn watch(&self) -> BoxStream<'static, Change> {
        let (tx, rx) = mpsc::channel::<Change>(WATCH_CHANNEL_CAP);
        let inner = Arc::clone(&self.inner);

        // poller exits when tx.send returns Err (consumer dropped the stream).
        tokio::spawn(async move {
            let mut last_revision: Option<String> = None;
            let mut tick = tokio::time::interval(inner.interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                tick.tick().await;
                let inner_ref = Arc::clone(&inner);
                let resolved = tokio::task::spawn_blocking(move || fetch::resolve_revision_blocking(&inner_ref)).await;
                let rev = match resolved {
                    Ok(Ok(rev)) => rev,
                    Ok(Err(e)) => {
                        warn!(target: "mars-definition-source-git", error = %e, "git poll: resolve failed");
                        continue;
                    }
                    Err(e) => {
                        warn!(target: "mars-definition-source-git", error = %e, "git poll: task join failed");
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
pub enum GitConfigError {
    /// `url` was empty or whitespace.
    #[error("git url must not be empty")]
    EmptyUrl,
    /// `path` was empty or whitespace.
    #[error("git path must not be empty")]
    EmptyPath,
    /// More than one of branch / tag / commit was set.
    #[error("git reference: exactly one of branch / tag / commit must be set")]
    AmbiguousReference,
    /// None of branch / tag / commit was set.
    #[error("git reference: one of branch / tag / commit must be set")]
    MissingReference,
    /// SSH bundle missing required key (one of `identity`, `identity.pub`, `known_hosts`).
    #[error("ssh auth bundle: {what}")]
    SshBundle {
        /// What was missing or malformed.
        what: &'static str,
    },
    /// Auth mode incompatible with the URL scheme.
    #[error("auth mode incompatible with url scheme: {what}")]
    SchemeMismatch {
        /// Stable label of the mismatch.
        what: &'static str,
    },
    /// mTLS bundle missing the matching key for a provided cert (or vice versa).
    #[error("tls bundle: {what}")]
    TlsBundle {
        /// What was missing or malformed.
        what: &'static str,
    },
}

impl From<GitConfigError> for DefinitionSourceError {
    fn from(e: GitConfigError) -> Self {
        DefinitionSourceError::Other { message: e.to_string() }
    }
}

pub(crate) fn map_git2_error(what: &'static str, e: git2::Error) -> DefinitionSourceError {
    use git2::ErrorClass;
    use git2::ErrorCode;
    match (e.class(), e.code()) {
        (_, ErrorCode::NotFound) => DefinitionSourceError::NotFound { what: what.to_string() },
        (ErrorClass::Http, _) | (ErrorClass::Net, _) | (ErrorClass::Ssh, _) | (ErrorClass::Ssl, _) => {
            // libgit2 reports 401/403 + ssh handshake rejections as Http/Ssh.
            // we use a coarse heuristic - "auth" / "credential" / "401" /
            // "403" in the message - to route to Auth vs Network. neither
            // libgit2 nor git2 exposes the http status explicitly here.
            let msg = e.message().to_ascii_lowercase();
            if msg.contains("authentication")
                || msg.contains("credential")
                || msg.contains("401")
                || msg.contains("403")
                || msg.contains("authorization")
            {
                DefinitionSourceError::Auth {
                    what: format!("{what}: {}", redact(&msg)),
                }
            } else {
                DefinitionSourceError::network(what, e)
            }
        }
        _ => DefinitionSourceError::network(what, e),
    }
}

// keep secrets out of error chains: libgit2 does not echo our credentials,
// but the http handshake message can include a URL with userinfo. strip the
// userinfo segment if it ever surfaces.
fn redact(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut chars = msg.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '/' && chars.peek() == Some(&'/') {
            out.push(c);
            out.push('/');
            chars.next();
            // consume up to the next '/' or whitespace, drop any '@'-prefixed userinfo
            let mut buf = String::new();
            for c2 in chars.by_ref() {
                if c2 == '/' || c2.is_whitespace() {
                    if let Some(idx) = buf.rfind('@') {
                        buf.replace_range(..=idx, "");
                    }
                    out.push_str(&buf);
                    out.push(c2);
                    break;
                }
                buf.push(c2);
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub(crate) fn map_tls_io(what: &'static str, e: std::io::Error) -> DefinitionSourceError {
    DefinitionSourceError::network(what, e)
}

pub(crate) fn write_secret_file(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    let p = dir.join(name);
    let mut f = std::fs::OpenOptions::new().create_new(true).write(true).open(&p)?;
    f.write_all(bytes)?;
    Ok(p)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;

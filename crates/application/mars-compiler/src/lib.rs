//! mars compiler use-case.
//!
//! Phase B (LAZARUS): the cell-keyed substrate is retired with the v3
//! manifest cut, and the snapshot / incremental implementations move to
//! Phase C. The crate's public API surface (`Compiler`, `Deps`,
//! `CompilerError`, `leader_lock_key`) stays in place so the bins keep
//! compiling; runs publish an empty v3 manifest in lieu of real output.
//! Tests that exercised the cell substrate are gone — the
//! Phase C rewrite will reintroduce them against page-keyed plans.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use mars_config::Config;
use mars_observability::Metrics;
use mars_source::{ChangeFeed, LeaderLock, LeaderLockGuard, Source};
use mars_store::{ManifestStore, ObjectStore, StoreError};
use mars_types::Manifest;
use tokio_util::sync::CancellationToken;

/// Capped exponential backoff schedule for retrying a transient publish.
const PUBLISH_RETRY_DELAYS: &[Duration] = &[
    Duration::from_millis(100),
    Duration::from_millis(500),
    Duration::from_secs(2),
    Duration::from_secs(8),
];

/// Deterministic 64-bit hash of the leader-lock key, reinterpreted as `i64`
/// for `pg_try_advisory_lock`. FNV-1a is stable across releases and has no
/// runtime dependency.
#[must_use]
pub fn leader_lock_key(name: &str) -> i64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for &b in name.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h as i64
}

/// Errors surfaced from the compiler.
#[derive(Debug, thiserror::Error)]
pub enum CompilerError {
    /// Source / change-feed adapter failed.
    #[error(transparent)]
    Source(#[from] mars_source::SourceError),
    /// Manifest / object store failed.
    #[error(transparent)]
    Store(#[from] mars_store::StoreError),
    /// Configuration was rejected during validation.
    #[error("config: {0}")]
    Config(#[from] mars_config::ConfigError),
    /// Another compiler instance holds the leader lock; this process should
    /// exit cleanly without producing output.
    #[error("another compiler instance is the leader")]
    NotLeader,
    /// Backend error while attempting to acquire the leader lock.
    #[error("leader lock acquisition failed: {source}")]
    LeaderLock {
        #[source]
        source: mars_source::SourceError,
    },
    /// Phase-C: substrate-bearing logic was retired with the v3 cut and is
    /// awaiting reimplementation. Carries a stable label naming the missing
    /// surface so callers and tests can match on it.
    #[error("legacy substrate retired: {what}")]
    LegacySubstrateRetired {
        /// Stable short label naming the unimplemented surface.
        what: &'static str,
    },
}

/// All ports the compiler depends on, bundled for easy composition by the bin.
pub struct Deps {
    /// Read-side source (geometry / attributes).
    pub source: Arc<dyn Source>,
    /// Subscription source for incremental updates.
    pub change_feed: Arc<dyn ChangeFeed>,
    /// Coordination lock so at most one compiler runs at a time.
    pub leader_lock: Arc<dyn LeaderLock>,
    /// Object store for artifact bodies.
    pub store: Arc<dyn ObjectStore>,
    /// Manifest pub/sub.
    pub manifest: Arc<dyn ManifestStore>,
    /// Service metrics handle.
    pub metrics: Metrics,
}

/// The compiler service.
pub struct Compiler {
    deps: Deps,
    config: Config,
}

impl Compiler {
    /// Build a `Compiler` from its ports and validated config.
    #[must_use]
    pub fn new(deps: Deps, config: Config) -> Self {
        Self { deps, config }
    }

    /// Acquire the leader lock and run a single snapshot compile, publishing
    /// an empty v3 manifest. Phase C reinstates the real snapshot pipeline.
    pub async fn run_snapshot_once(&self, shutdown: CancellationToken) -> Result<u64, CompilerError> {
        let _guard = self.acquire_leader().await?;
        let manifest = Manifest::empty(1, self.config.service.name.clone());
        let v = publish_with_retry(self.deps.manifest.as_ref(), &manifest, &self.deps.metrics, &shutdown).await?;
        tracing::info!(version = v, "compiler: empty v3 manifest published (phase-b stub)");
        Ok(v)
    }

    /// Long-running service mode. Phase B stub: bootstraps with an empty
    /// manifest if none exists, then idles awaiting `shutdown`. Change-feed
    /// consumption returns in Phase C with page-keyed dirty propagation.
    pub async fn run(&self, shutdown: CancellationToken) -> Result<(), CompilerError> {
        let _guard = self.acquire_leader().await?;
        if self.deps.manifest.current().await?.is_none() {
            let manifest = Manifest::empty(1, self.config.service.name.clone());
            publish_with_retry(self.deps.manifest.as_ref(), &manifest, &self.deps.metrics, &shutdown).await?;
        }
        shutdown.cancelled().await;
        Ok(())
    }

    async fn acquire_leader(&self) -> Result<Box<dyn LeaderLockGuard>, CompilerError> {
        let key = leader_lock_key(&self.config.service.name);
        match self
            .deps
            .leader_lock
            .try_acquire(key)
            .await
            .map_err(|source| CompilerError::LeaderLock { source })?
        {
            Some(g) => Ok(g),
            None => {
                tracing::info!(service = %self.config.service.name, "compiler: not leader, exiting");
                Err(CompilerError::NotLeader)
            }
        }
    }
}

async fn publish_with_retry(
    manifest_store: &dyn ManifestStore,
    manifest: &Manifest,
    metrics: &Metrics,
    shutdown: &CancellationToken,
) -> Result<u64, CompilerError> {
    let mut delays = PUBLISH_RETRY_DELAYS.iter();
    loop {
        let reason = match manifest_store.publish(manifest).await {
            Ok(v) => return Ok(v),
            Err(StoreError::Transient(r)) => r,
            Err(e) => return Err(CompilerError::Store(e)),
        };
        let Some(d) = delays.next() else {
            return Err(CompilerError::Store(StoreError::Transient(reason)));
        };
        metrics.inc_compiler_publish_retries();
        tracing::warn!(
            version = manifest.version,
            delay_ms = d.as_millis() as u64,
            reason,
            "compiler: transient publish failure; retrying"
        );
        tokio::select! {
            _ = shutdown.cancelled() => return Err(CompilerError::Store(StoreError::Transient(reason))),
            _ = tokio::time::sleep(*d) => {}
        }
    }
}

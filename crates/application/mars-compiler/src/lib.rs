//! mars compiler use-case.
//!
//! Page-keyed substrate. The compiler builds artifacts keyed by
//! `(binding, decimation level, hilbert page range)`; `Compiler::run` is
//! the long-running service entry point and orchestrates a snapshot
//! bootstrap (when no manifest exists) followed by per-`compiler.window`
//! incremental cycles fed by [`mars_source::ChangeFeed`].

#![forbid(unsafe_code)]

pub mod class_eval;
pub mod decimate;
pub mod disk_governor;
mod error;
pub mod external_sort;
pub mod hilbert;
pub mod incremental;
pub mod memory_governor;
pub mod page_plan;
pub mod plan;
pub mod polylabel;
pub mod rebalance;
pub mod reconcile;
pub mod render;
pub mod route_index;
pub(crate) mod scratch_codec;
mod service;
pub mod sidecar;
pub mod sidecar_arena;
mod snapshot_pipeline;
mod sources;
pub(crate) mod spill;
mod stages;
pub mod testing;

pub use error::CompilerError;
pub use snapshot_pipeline::run_snapshot_from_plan;
pub use sources::SourceRegistry;

use std::collections::HashMap;
use std::sync::Arc;

use mars_config::Config;
use mars_observability::{Metrics, rebalance_outcome};
use mars_source::{ChangeBatch, ChangeFeed, LeaderLock, LeaderLockGuard, Source};
use mars_store::{ManifestStore, ObjectStore};
use mars_types::BindingId;
use tokio_util::sync::CancellationToken;

use crate::service::{CollectOutcome, collect_batches, leader_lock_key};

/// All ports the compiler depends on, bundled for easy composition by the bin.
pub struct Deps {
    /// Registry of read-side sources keyed by their configured id. Each
    /// binding plan carries the id of the source that feeds it.
    pub sources: Arc<SourceRegistry>,
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

impl Deps {
    /// Route a binding plan to its source via the registry. Returns
    /// [`CompilerError::UnknownSource`] when the binding's declared source id
    /// is not registered (a configuration / wiring bug).
    pub fn source_for(&self, binding: &plan::BindingPlan) -> Result<Arc<dyn Source>, CompilerError> {
        self.sources
            .get(&binding.source_id)
            .ok_or_else(|| CompilerError::UnknownSource {
                binding: binding.binding_id.as_str().to_string(),
                source_id: binding.source_id.as_str().to_string(),
            })
    }
}

/// The compiler service.
pub struct Compiler {
    deps: Deps,
    config: Config,
    /// Per-binding cycle counter that drives the periodic reconciliation hook
    /// in [`Self::run_cycle_once`]. Hydrated lazily on first observation
    /// per binding from `prior.bindings[*].cycles_since_reconcile`, so the
    /// cadence survives leader handover / process restart; written back
    /// into each cycle's manifest via [`stages::cycle::merge`].
    ///
    /// `parking_lot::Mutex` rather than `tokio::sync::RwLock`: the
    /// critical section is purely sync (no `.await` under the guard) and
    /// every access mutates, so the async-aware RwLock would just be
    /// noise. infallible `lock()` keeps the call site clean.
    pub(crate) cycle_counter: parking_lot::Mutex<HashMap<BindingId, u32>>,
}

impl Compiler {
    /// Build a `Compiler` from its ports and validated config.
    #[must_use]
    pub fn new(deps: Deps, config: Config) -> Self {
        Self {
            deps,
            config,
            cycle_counter: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    /// Acquire the leader lock and run a single snapshot compile, publishing
    /// the resulting v3 manifest.
    pub async fn run_snapshot_once(&self, shutdown: CancellationToken) -> Result<u64, CompilerError> {
        let _guard = self.acquire_leader().await?;
        self.apply_snapshot(&shutdown).await
    }

    async fn apply_snapshot(&self, shutdown: &CancellationToken) -> Result<u64, CompilerError> {
        stages::snapshot::run(self, shutdown).await
    }

    /// Apply one or more change batches as a single incremental cycle and
    /// publish the resulting v3 manifest. Returns the published version.
    /// The caller (typically [`Self::run`]) is responsible for sourcing
    /// `batches` from a [`mars_source::ChangeSubscription`] and acking
    /// downstream once this returns.
    ///
    /// Cycle entry point for the page-keyed substrate.
    pub async fn run_cycle_once(&self, batches: Vec<ChangeBatch>) -> Result<u64, CompilerError> {
        let _guard = self.acquire_leader().await?;
        self.apply_cycle(batches, &CancellationToken::new()).await
    }

    async fn apply_cycle(&self, batches: Vec<ChangeBatch>, shutdown: &CancellationToken) -> Result<u64, CompilerError> {
        stages::cycle::run(self, batches, shutdown).await
    }

    /// Run one opportunistic rebalance pass over the current manifest.
    /// Identifies pages outside the size band or with dilated bboxes via
    /// [`crate::rebalance::rebalance_candidates`] and rewrites them through
    /// [`crate::render::execute_rebalance`]. No-op when the manifest is
    /// already balanced. Acquires the leader lock; intended for operator-
    /// driven invocation. The periodic dispatch in [`Self::run`] calls
    /// [`Self::rebalance_locked`] directly because it already holds the lock.
    pub async fn run_rebalance_once(&self) -> Result<u64, CompilerError> {
        let _guard = self.acquire_leader().await?;
        self.rebalance_locked().await
    }

    /// Body of one rebalance pass without leader-lock acquisition. Caller
    /// must already hold the lock (operator path acquires in
    /// [`Self::run_rebalance_once`]; service path holds it for the lifetime
    /// of [`Self::run`]).
    async fn rebalance_locked(&self) -> Result<u64, CompilerError> {
        stages::rebalance::run(self).await
    }

    /// Long-running service mode. Acquires the leader lock, runs a snapshot
    /// bootstrap if no prior manifest exists, then drives one incremental
    /// cycle per `compiler.window`, sourcing batches from
    /// [`mars_source::ChangeFeed::subscribe`]. Returns when `shutdown` fires
    /// or the change feed closes cleanly.
    pub async fn run(&self, shutdown: CancellationToken) -> Result<(), CompilerError> {
        let _guard = self.acquire_leader().await?;

        if self.deps.manifest.current().await?.is_none() {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => return Ok(()),
                res = self.apply_snapshot(&shutdown) => { res?; }
            }
            if shutdown.is_cancelled() {
                return Ok(());
            }
        }

        let mut sub = self.deps.change_feed.subscribe().await.map_err(CompilerError::Source)?;

        let window = self.config.compiler.window_dur()?;

        // opportunistic rebalance schedule. checked between cycles; we already
        // hold the leader lock for the lifetime of run() so rebalance_locked()
        // is the right entry point (acquire_leader is non-reentrant).
        let rebalance_enabled = self.config.compiler.rebalance.enabled;
        let rebalance_period = self.config.compiler.rebalance.window_dur()?;
        let mut next_rebalance = tokio::time::Instant::now() + rebalance_period;

        loop {
            let batches = match collect_batches(&mut *sub, window, &shutdown).await? {
                CollectOutcome::Shutdown => {
                    let _ = sub.shutdown().await;
                    return Ok(());
                }
                CollectOutcome::FeedClosed => {
                    tracing::info!("compiler: change feed closed; exiting service loop");
                    let _ = sub.shutdown().await;
                    return Ok(());
                }
                CollectOutcome::Batches(b) => b,
            };

            if !batches.is_empty() {
                let last_version = batches.iter().rev().find_map(|b| b.source_version.clone());
                let v = self.apply_cycle(batches, &shutdown).await?;
                sub.acknowledge(last_version.as_deref())
                    .await
                    .map_err(CompilerError::Source)?;
                tracing::info!(version = v, "compiler: cycle manifest published");
            }

            if rebalance_enabled && tokio::time::Instant::now() >= next_rebalance {
                match self.rebalance_locked().await {
                    Ok(v) => {
                        tracing::info!(version = v, "compiler: rebalance completed");
                        self.deps.metrics.inc_compiler_rebalance_run(rebalance_outcome::OK);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "compiler: rebalance failed");
                        self.deps.metrics.inc_compiler_rebalance_run(rebalance_outcome::ERROR);
                    }
                }
                next_rebalance = tokio::time::Instant::now() + rebalance_period;
            }

            if shutdown.is_cancelled() {
                let _ = sub.shutdown().await;
                return Ok(());
            }
        }
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

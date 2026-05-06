//! mars compiler use-case. Phase 0 ships the snapshot path only: read config,
//! enumerate cells, fetch rows, build source + layer artifacts, publish a
//! manifest. The change-feed dependency is held for forward-compat (Phase 1).

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Instant;

use futures_util::stream::{self, StreamExt};
use mars_config::Config;
use mars_observability::Metrics;
use mars_source::{ChangeFeed, LeaderLock, LeaderLockGuard, Source};
use mars_store::{ManifestStore, ObjectStore};
use mars_types::Manifest;
use tokio_util::sync::CancellationToken;

pub mod class;
pub mod incremental;
pub mod plan;
pub mod snapshot;
pub mod wkb;

/// Deterministic 64-bit hash of the leader-lock key, reinterpreted as `i64`
/// for `pg_try_advisory_lock`. FNV-1a is stable across releases and has no
/// runtime dependency; `DefaultHasher` is unsuitable because its seed is
/// process-local and may change between rust releases.
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

/// Default number of concurrent in-flight cell builds in the snapshot driver.
/// The connection pool size on the source side becomes the real ceiling, but
/// 16 is a reasonable starting point for typical postgres pools.
const DEFAULT_SNAPSHOT_CONCURRENCY: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum CompilerError {
    #[error(transparent)]
    Source(#[from] mars_source::SourceError),
    #[error(transparent)]
    Store(#[from] mars_store::StoreError),
    #[error(transparent)]
    Plan(#[from] plan::PlanError),
    #[error(transparent)]
    Wkb(#[from] crate::wkb::WkbError),
    #[error(transparent)]
    Artifact(#[from] mars_artifact::ArtifactError),
    #[error(transparent)]
    Expr(#[from] mars_expr::ExprError),
    #[error("build task panicked: {reason}")]
    BuildTaskPanic { reason: String },
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
    #[error("config: {0}")]
    Config(#[from] mars_config::ConfigError),
}

/// All ports the compiler depends on, bundled for easy composition by the bin.
pub struct Deps {
    pub source: Arc<dyn Source>,
    pub change_feed: Arc<dyn ChangeFeed>,
    pub leader_lock: Arc<dyn LeaderLock>,
    pub store: Arc<dyn ObjectStore>,
    pub manifest: Arc<dyn ManifestStore>,
    pub metrics: Metrics,
}

/// The compiler service.
pub struct Compiler {
    deps: Deps,
    config: Config,
}

impl Compiler {
    #[must_use]
    pub fn new(deps: Deps, config: Config) -> Self {
        Self { deps, config }
    }

    /// Acquire the leader lock and run a single snapshot compile, publishing
    /// manifest version 1. Used by `mars-compile`, tests, and as the bootstrap
    /// step inside [`Compiler::run`] when no manifest exists yet.
    pub async fn run_snapshot_once(&self, shutdown: CancellationToken) -> Result<u64, CompilerError> {
        let _guard = self.acquire_leader().await?;
        Ok(self.snapshot_inner(shutdown).await?.version)
    }

    /// Long-running service mode: bootstrap with a snapshot if no manifest
    /// exists, then consume committed change batches in `compiler.window`
    /// chunks, rebuilding only dirty source cells and republishing a merged
    /// manifest at version+1 per cycle. SPEC §8.3.
    ///
    /// Acknowledgement is tied to manifest durability: the subscription cursor
    /// only advances after `publish` succeeds. Crashes between `next_batch`
    /// and `acknowledge` re-deliver the window on reconnect.
    pub async fn run(&self, shutdown: CancellationToken) -> Result<(), CompilerError> {
        let _guard = self.acquire_leader().await?;

        let mut prev = match self.deps.manifest.current().await? {
            Some(m) => m,
            None => self.snapshot_inner(shutdown.clone()).await?,
        };

        // a configured-but-unimplemented feed is a hard error in service mode;
        // silently exiting would leave the manifest frozen forever.
        let mut sub = self.deps.change_feed.subscribe().await?;

        let window = self.config.compiler.window_dur()?;
        let plan = plan::build_plan(&self.config)?;

        loop {
            if shutdown.is_cancelled() {
                return Ok(());
            }
            let mut batches: Vec<mars_source::ChangeBatch> = Vec::new();
            let deadline = tokio::time::Instant::now() + window;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return Ok(()),
                    _ = tokio::time::sleep_until(deadline) => break,
                    next = sub.next_batch() => match next {
                        Some(Ok(b)) => batches.push(b),
                        Some(Err(e)) => return Err(e.into()),
                        None => {
                            tracing::info!("compiler: change feed closed");
                            return Ok(());
                        }
                    }
                }
            }
            if batches.is_empty() {
                continue;
            }

            let source_version = batches.iter().rev().find_map(|b| b.source_version.clone());

            let dirty = incremental::dirty_cells_for(&batches, &plan);
            let next_plan = incremental::filter_plan(&plan, &dirty);
            if next_plan.sources.is_empty() {
                // window had no events touching the configured plan; advance
                // the cursor so the upstream slot does not retain logs but
                // skip publishing — nothing changed.
                sub.acknowledge(source_version.as_deref()).await?;
                continue;
            }
            let rebuild_start = Instant::now();
            // rebuild and publish before acking. on failure we return without
            // calling acknowledge, so the next subscription replays the window.
            let rebuild = self.execute_plan(next_plan, shutdown.clone()).await?;
            self.deps.metrics.inc_compiler_change_events();
            self.deps.metrics.inc_compiler_dirty_cells(dirty.cells.len() as u64);
            self.deps
                .metrics
                .observe_compiler_rebuild_duration(rebuild_start.elapsed());

            let next_version = prev.version + 1;
            let merged = incremental::merge_manifest(
                &prev,
                next_version,
                &self.config.service.name,
                rebuild,
                &dirty,
                source_version,
            );
            self.deps.manifest.publish(&merged).await?;
            sub.acknowledge(merged.source_version.as_deref()).await?;
            tracing::info!(
                version = merged.version,
                dirty_cells = dirty.cells.len(),
                "compiler: incremental manifest published",
            );
            prev = merged;
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

    async fn snapshot_inner(&self, shutdown: CancellationToken) -> Result<Manifest, CompilerError> {
        let plan = plan::build_plan(&self.config)?;
        tracing::info!(
            sources = plan.sources.len(),
            layers = plan.layers.len(),
            "compiler: snapshot plan built",
        );
        self.deps.metrics.inc_compiler_change_events();
        self.deps.metrics.inc_compiler_dirty_cells(plan.sources.len() as u64);
        self.deps.metrics.set_compiler_window_lag(std::time::Duration::ZERO);

        let rebuild_start = Instant::now();
        let output = self.execute_plan(plan, shutdown).await?;

        let manifest = Manifest::new(
            1,
            self.config.service.name.clone(),
            output.source_artifacts,
            output.layer_artifacts,
            None,
            output.empty_layer_cells,
        );
        let v = self.deps.manifest.publish(&manifest).await?;
        self.deps
            .metrics
            .observe_compiler_rebuild_duration(rebuild_start.elapsed());
        tracing::info!(version = v, "compiler: manifest published");
        Ok(manifest)
    }

    /// Drive one plan through the per-source-cell rebuild pipeline. Concurrency
    /// is bounded by [`DEFAULT_SNAPSHOT_CONCURRENCY`]; callers handle metric
    /// accounting and manifest publication.
    async fn execute_plan(
        &self,
        plan: plan::Plan,
        shutdown: CancellationToken,
    ) -> Result<snapshot::SnapshotOutput, CompilerError> {
        let mut output = snapshot::SnapshotOutput::default();
        let source = self.deps.source.clone();
        let store = self.deps.store.clone();

        let dependents = plan.dependents_by_source();
        let plan::Plan { sources, layers } = plan;
        let layer_arcs: Vec<Arc<plan::LayerTask>> = layers.into_iter().map(Arc::new).collect();
        let units: Vec<(Arc<plan::SourceTask>, Vec<Arc<plan::LayerTask>>)> = sources
            .into_iter()
            .zip(dependents)
            .map(|(s, deps)| {
                let dep_arcs: Vec<Arc<plan::LayerTask>> = deps.into_iter().map(|i| layer_arcs[i].clone()).collect();
                (Arc::new(s), dep_arcs)
            })
            .collect();

        let mut stream = stream::iter(units)
            .map(|(task, deps)| {
                let source = source.clone();
                let store = store.clone();
                async move { snapshot::run_source_cell(&task, &deps, &source, &store).await }
            })
            .buffer_unordered(DEFAULT_SNAPSHOT_CONCURRENCY);
        while let Some(result) = stream.next().await {
            if shutdown.is_cancelled() {
                return Ok(output);
            }
            output.extend(result?);
        }
        Ok(output)
    }
}

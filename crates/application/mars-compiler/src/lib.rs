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

    /// Run one snapshot pass. The change-feed dependency is held but not
    /// subscribed in Phase 0 (SPEC §8.2; deferred to Phase 1).
    pub async fn run(&self, shutdown: CancellationToken) -> Result<(), CompilerError> {
        // singleton enforcement: hold the leader lock for the whole run.
        let key = leader_lock_key(&self.config.service.name);
        let _guard: Box<dyn LeaderLockGuard> = match self
            .deps
            .leader_lock
            .try_acquire(key)
            .await
            .map_err(|source| CompilerError::LeaderLock { source })?
        {
            Some(g) => g,
            None => {
                tracing::info!(service = %self.config.service.name, "compiler: not leader, exiting");
                return Err(CompilerError::NotLeader);
            }
        };

        tracing::warn!("phase-1: change feed deferred");
        let _ = &self.deps.change_feed;

        if shutdown.is_cancelled() {
            return Ok(());
        }

        let tasks = plan::build_plan(&self.config)?;
        tracing::info!(task_count = tasks.len(), "compiler: snapshot plan built");
        self.deps.metrics.inc_compiler_change_events();
        self.deps.metrics.inc_compiler_dirty_cells(tasks.len() as u64);
        self.deps.metrics.set_compiler_window_lag(std::time::Duration::ZERO);

        let mut output = snapshot::SnapshotOutput::default();
        let source = self.deps.source.clone();
        let store = self.deps.store.clone();
        let rebuild_start = Instant::now();
        // flatten (task × cell) so the unit of parallelism is one cell. with
        // a single layer/source/band the previous task-level granularity was
        // effectively serial; per-cell fan-out keeps the postgres pool busy.
        // the per-task `Arc<BuildTask>` carries shared compiled-class state.
        let units = tasks
            .into_iter()
            .flat_map(|t| {
                let cells = t.cells.clone();
                let task = Arc::new(t);
                cells.into_iter().map(move |c| (task.clone(), c))
            })
            .collect::<Vec<_>>();
        let mut stream = stream::iter(units)
            .map(|(task, cell)| {
                let source = source.clone();
                let store = store.clone();
                async move { snapshot::run_cell(&task, &cell, &source, &store).await }
            })
            .buffer_unordered(DEFAULT_SNAPSHOT_CONCURRENCY);
        while let Some(result) = stream.next().await {
            if shutdown.is_cancelled() {
                return Ok(());
            }
            let part = result?;
            output.extend(part);
        }

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
        Ok(())
    }
}

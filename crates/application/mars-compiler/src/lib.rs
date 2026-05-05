//! mars compiler use-case. Phase 0 ships the snapshot path only: read config,
//! enumerate cells, fetch rows, build source + layer artifacts, publish a
//! manifest. The change-feed dependency is held for forward-compat (Phase 1).

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Instant;

use futures_util::stream::{self, StreamExt};
use mars_config::Config;
use mars_observability::Metrics;
use mars_source::{ChangeFeed, Source};
use mars_store::{ManifestStore, ObjectStore};
use mars_types::Manifest;
use tokio_util::sync::CancellationToken;

pub mod class;
pub mod plan;
pub mod snapshot;
pub mod wkb;

const SNAPSHOT_CONCURRENCY: usize = 4;

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
}

/// All ports the compiler depends on, bundled for easy composition by the bin.
pub struct Deps {
    pub source: Arc<dyn Source>,
    pub change_feed: Arc<dyn ChangeFeed>,
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
        let mut stream = stream::iter(tasks)
            .map(|task| {
                let source = source.clone();
                let store = store.clone();
                async move { snapshot::run_task(&task, &source, &store).await }
            })
            .buffer_unordered(SNAPSHOT_CONCURRENCY);
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
        self.deps.metrics.observe_compiler_rebuild_duration(rebuild_start.elapsed());
        tracing::info!(version = v, "compiler: manifest published");
        Ok(())
    }
}

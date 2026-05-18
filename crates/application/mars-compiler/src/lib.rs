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
pub mod sidecar;
pub mod sidecar_arena;
mod sources;
pub(crate) mod spill;
mod stages;
pub mod testing;

pub use error::CompilerError;
pub use sources::SourceRegistry;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use mars_config::Config;
use mars_observability::{Metrics, rebalance_outcome};
use mars_source::{ChangeBatch, ChangeFeed, ChangeSubscription, LeaderLock, LeaderLockGuard, Source};
use mars_store::{ManifestStore, ObjectStore};
use mars_types::{BindingId, Manifest};
use tokio_util::sync::CancellationToken;

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

/// Snapshot orchestrator built on the unified compile pipeline:
/// per binding, open a `CompileSession`, run [`crate::page_plan::compute_page_plan`]
/// for pass 1, hand the resulting `PagePlan` to
/// [`crate::render::rebuild_binding_from_plan`] for pass 2, fold the
/// emitted artifacts into a fresh `Manifest`. Bindings compile concurrently
/// up to `binding_parallelism` (each holds one pooled connection in
/// `REPEATABLE READ`); the operator must size `source.pool.max_size`
/// accordingly. Returns the manifest for the caller to publish.
#[allow(clippy::too_many_arguments)]
pub async fn run_snapshot_from_plan(
    deps: &Deps,
    bootstrap: &plan::BootstrapPlan,
    service_name: String,
    manifest_version: u64,
    working_set_bytes: u64,
    plan_budget_bytes: u64,
    in_flight_budget_bytes: u64,
    binding_parallelism: usize,
    spill_dir: &std::path::Path,
    spill_open_file_limit: usize,
    governor: &memory_governor::MemoryGovernor,
    disk_governor: &disk_governor::DiskGovernor,
) -> Result<Manifest, CompilerError> {
    use futures_util::StreamExt;
    use mars_source::{SourceBinding as PortBinding, SourceCollectionId};
    use mars_types::{LayerSidecarEntry, MANIFEST_FORMAT_VERSION, PageEntry};

    use crate::render::BindingOutput;

    let parallelism = binding_parallelism.max(1);

    async fn compile_one(
        deps: &Deps,
        bootstrap: &plan::BootstrapPlan,
        binding_plan: &plan::BindingPlan,
        working_set_bytes: u64,
        plan_budget_bytes: u64,
        in_flight_budget_bytes: u64,
        spill_dir: &std::path::Path,
        spill_open_file_limit: usize,
        governor: &memory_governor::MemoryGovernor,
        disk_governor: &disk_governor::DiskGovernor,
    ) -> Result<BindingOutput, CompilerError> {
        let port_binding = PortBinding::new(
            SourceCollectionId::new(binding_plan.binding_id.as_str()),
            binding_plan.source_table.clone(),
            binding_plan.geometry_field.clone(),
            binding_plan.id_field.as_deref().unwrap_or("id"),
            binding_plan.attributes.clone(),
            binding_plan.native_crs.clone(),
        )?
        .with_filter(binding_plan.filter.clone())
        .with_dsn(binding_plan.dsn.clone());
        let started = std::time::Instant::now();
        tracing::info!(
            target: "mars_compiler::compile",
            binding = %binding_plan.binding_id,
            "compile.binding.start",
        );
        let source = deps.source_for(binding_plan)?;
        let mut session = source.open_compile_session(&port_binding).await?;
        let work = async {
            let page_plan =
                page_plan::compute_page_plan(session.as_mut(), binding_plan, plan_budget_bytes, spill_dir).await?;
            render::rebuild_binding_from_plan(
                deps,
                bootstrap,
                binding_plan,
                &page_plan,
                session.as_mut(),
                working_set_bytes,
                in_flight_budget_bytes,
                spill_dir,
                spill_open_file_limit,
                governor,
                disk_governor,
            )
            .await
        }
        .await;
        match work {
            Ok(out) => {
                session.commit().await?;
                let pages: usize = out.pages.len();
                let levels = out.meta.levels.len();
                tracing::info!(
                    target: "mars_compiler::compile",
                    binding = %binding_plan.binding_id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    pages = pages,
                    levels = levels,
                    feature_count_total = out.meta.feature_count_total,
                    "compile.binding.end",
                );
                Ok(out)
            }
            Err(err) => {
                if let Err(rb) = session.rollback().await {
                    tracing::warn!(error = %rb, "compile session rollback failed");
                }
                tracing::info!(
                    target: "mars_compiler::compile",
                    binding = %binding_plan.binding_id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    error = %err,
                    "compile.binding.end",
                );
                Err(err)
            }
        }
    }

    let mut pending = futures_util::stream::FuturesUnordered::new();
    let mut iter = bootstrap.bindings.iter();
    let mut outputs: Vec<BindingOutput> = Vec::with_capacity(bootstrap.bindings.len());
    loop {
        while pending.len() < parallelism
            && let Some(binding_plan) = iter.next()
        {
            pending.push(compile_one(
                deps,
                bootstrap,
                binding_plan,
                working_set_bytes,
                plan_budget_bytes,
                in_flight_budget_bytes,
                spill_dir,
                spill_open_file_limit,
                governor,
                disk_governor,
            ));
        }
        match pending.next().await {
            Some(Ok(out)) => outputs.push(out),
            Some(Err(err)) => return Err(err),
            None => break,
        }
    }

    let mut bindings_meta: Vec<mars_types::BindingMetadata> = Vec::with_capacity(outputs.len());
    let mut pages_meta: Vec<PageEntry> = Vec::new();
    let mut class_sidecars: Vec<LayerSidecarEntry> = Vec::new();
    let mut label_sidecars: Vec<LayerSidecarEntry> = Vec::new();

    for mut out in outputs {
        bindings_meta.push(out.meta);
        pages_meta.append(&mut out.pages);
        class_sidecars.append(&mut out.class_sidecars);
        label_sidecars.append(&mut out.label_sidecars);
    }

    // stable manifest ordering under concurrent binding compilation.
    bindings_meta.sort_by(|a, b| a.binding_id.as_str().cmp(b.binding_id.as_str()));
    pages_meta.sort_by(|a, b| {
        a.key
            .binding_id
            .as_str()
            .cmp(b.key.binding_id.as_str())
            .then_with(|| a.key.level.cmp(&b.key.level))
            .then_with(|| a.hilbert_range.0.cmp(&b.hilbert_range.0))
    });
    let sidecar_cmp = |a: &LayerSidecarEntry, b: &LayerSidecarEntry| {
        a.layer_id
            .as_str()
            .cmp(b.layer_id.as_str())
            .then_with(|| a.page_key.binding_id.as_str().cmp(b.page_key.binding_id.as_str()))
            .then_with(|| a.page_key.level.cmp(&b.page_key.level))
            .then_with(|| a.page_key.page_id.cmp(&b.page_key.page_id))
    };
    class_sidecars.sort_by(sidecar_cmp);
    label_sidecars.sort_by(sidecar_cmp);

    Ok(Manifest {
        format_version: MANIFEST_FORMAT_VERSION,
        version: manifest_version,
        service: service_name,
        created_at: std::time::SystemTime::now(),
        bindings: bindings_meta,
        pages: pages_meta,
        class_sidecars,
        label_sidecars,
        style_artifact: None,
        image_artifact: None,
        raster_layers: bootstrap.raster_layers.clone(),
        source_version: None,
        epoch: manifest_version,
    })
}

enum CollectOutcome {
    Batches(Vec<ChangeBatch>),
    FeedClosed,
    Shutdown,
}

/// Drain the subscription until `window` elapses or shutdown fires. Returns
/// every batch that arrived in the window. An empty batch list is a normal
/// idle window.
async fn collect_batches(
    sub: &mut dyn ChangeSubscription,
    window: Duration,
    shutdown: &CancellationToken,
) -> Result<CollectOutcome, CompilerError> {
    let deadline = tokio::time::Instant::now() + window;
    let mut batches: Vec<ChangeBatch> = Vec::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(CollectOutcome::Shutdown),
            _ = tokio::time::sleep_until(deadline) => return Ok(CollectOutcome::Batches(batches)),
            next = sub.next_batch() => match next {
                None => return Ok(CollectOutcome::FeedClosed),
                Some(Err(e)) => return Err(CompilerError::Source(e)),
                Some(Ok(b)) => batches.push(b),
            }
        }
    }
}


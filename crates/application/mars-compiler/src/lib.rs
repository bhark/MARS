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
pub(crate) mod spill;
mod stages;
pub mod testing;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use mars_config::Config;
use mars_observability::{Metrics, rebalance_outcome};
use mars_source::{ChangeBatch, ChangeFeed, ChangeSubscription, LeaderLock, LeaderLockGuard, Source};
use mars_store::{ManifestStore, ObjectStore, StoreError};
use mars_types::{BindingId, Manifest};
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
    /// incremental dirty-page identification failed.
    #[error(transparent)]
    Incremental(#[from] incremental::IncrementalError),
    /// Bootstrap plan construction rejected the config.
    #[error(transparent)]
    Plan(#[from] plan::PlanError),
    /// Page-membership sidecar codec failure.
    #[error(transparent)]
    Sidecar(#[from] sidecar::SidecarError),
    /// WKB decode failed for a feature row.
    #[error(transparent)]
    Wkb(#[from] mars_artifact::WkbError),
    /// Per-row attribute codec failed.
    #[error(transparent)]
    Attr(#[from] mars_artifact::AttrError),
    /// mars-artifact writer/reader error during page or sidecar assembly.
    #[error(transparent)]
    Artifact(#[from] mars_artifact::ArtifactError),
    /// `PageKey` / `LayerSidecarEntry` object-key construction rejected
    /// component characters.
    #[error(transparent)]
    ArtifactKey(#[from] mars_types::ArtifactKeyError),
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
    /// No prior manifest exists; the operator must run the snapshot
    /// bootstrap once before incremental cycles or rebalance can proceed.
    #[error("no prior manifest; run snapshot bootstrap first ({context})")]
    NoPriorManifest {
        /// Stable label naming the call site that needed a prior manifest.
        context: &'static str,
    },
    /// Internal invariant violated mid-cycle: state expected in plan or
    /// manifest was absent. Indicates a code bug or out-of-sync manifest;
    /// not user-recoverable.
    #[error("internal invariant: {what}")]
    InvariantViolation {
        /// Stable short label naming the violated invariant.
        what: &'static str,
    },
    /// A row's attribute payload exceeds the per-row codec's maximum.
    #[error("row attributes too large: feature {feature_id} = {bytes} bytes (max {max} bytes)")]
    RowAttributesTooLarge {
        /// Offending feature id.
        feature_id: u64,
        /// Encoded attribute byte length.
        bytes: usize,
        /// Codec maximum.
        max: usize,
    },
    /// Binding identifier contains characters that would break object keys.
    #[error("binding id contains forbidden characters (/ or NUL): {binding}")]
    InvalidBindingId {
        /// Offending binding identifier.
        binding: String,
    },
    /// Per-page hydrated working set crossed the configured ceiling. The
    /// rebuild path fetches a bounded feature-id set per page and asserts
    /// the hydrated rows stay under `compile_page_working_set_bytes`.
    /// Bailout: lift the budget, or split the binding.
    #[error(
        "compile working-set exceeded: binding {binding}{} accumulated {observed_bytes} bytes \
         (budget {budget_bytes}). lift compiler.compile_page_working_set_bytes or split the binding.",
        page_id.map(|p| format!(" page {}", p.get())).unwrap_or_default()
    )]
    ScratchBudgetExceeded {
        /// Affected binding id.
        binding: String,
        /// Affected page id when the breach was observed inside a per-page
        /// hydration loop. `None` when the breach is binding-scoped.
        page_id: Option<mars_types::PageId>,
        /// Observed accumulated bytes at the point the budget was crossed.
        observed_bytes: u64,
        /// Configured working-set ceiling.
        budget_bytes: u64,
    },
    /// Pass-2 in-flight page buffers crossed the per-binding ceiling.
    /// Streamed rows are bucketed into the planned pages and pages
    /// eager-flush on completion, but rows arriving in non-spatial-cluster
    /// order can leave many pages partially full at once. Resolution: lift
    /// `compiler.compile_in_flight_pages_budget_bytes`, lower
    /// `compiler.compile_binding_parallelism`, or cluster the source table
    /// on a hilbert / spatial index so completed pages flush sooner.
    #[error(
        "compile in-flight pages budget exceeded: binding {binding} accumulated {observed_bytes} bytes \
         (budget {budget_bytes}). lift compiler.compile_in_flight_pages_budget_bytes, lower \
         compiler.compile_binding_parallelism, or cluster the source table to flush pages sooner."
    )]
    CompileMemoryBudgetExceeded {
        /// Affected binding id.
        binding: String,
        /// Observed accumulated bytes at the point the budget was crossed.
        observed_bytes: u64,
        /// Configured in-flight pages budget.
        budget_bytes: u64,
    },
    /// Pass-2 disk-spill primitive failed (filesystem I/O, malformed
    /// spill file). Spill is a fallback path inside one process; failures
    /// are not user-recoverable and surface as a typed compile error.
    #[error("compile spill: {what}: {source}")]
    Spill {
        /// Stable short label naming the failed step (open, write, flush,
        /// drain, etc).
        what: &'static str,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Disk admission semaphore closed mid-compile. The compiler holds
    /// the only governor for the duration of a cycle, so closure means
    /// the process is unwinding; surface it as a typed error rather than
    /// silently dropping the spill write.
    #[error("disk governor acquire failed: {source}")]
    DiskGovernor {
        #[source]
        source: tokio::sync::AcquireError,
    },
    /// Pass-1 page planning observed more rows than the in-memory plan
    /// budget allows. Trips before the planner allocates beyond its
    /// configured ceiling so the operator gets a clean ceiling rather than
    /// an OOM. Resolution: split the binding, lift the budget, or add a
    /// fixed-record pass-1 spill primitive (planned follow-up - track via
    /// repository issue tracker once landed).
    #[error(
        "bootstrap plan exceeds plan budget: binding {binding} observed {observed_rows} rows, \
         which would exceed plan budget {budget_bytes} bytes. resolution: split the binding, \
         lift compiler.compile_plan_budget_bytes, or add a fixed-record pass-1 spill primitive."
    )]
    BootstrapPlanTooLarge {
        /// Affected binding id.
        binding: String,
        /// Number of rows observed at the point the budget was crossed.
        observed_rows: u64,
        /// Configured plan budget (bytes).
        budget_bytes: u64,
    },
    /// Compiler emit produced a class-assignment sidecar whose slot count
    /// does not match the geometry payload's slot count, on a layer with at
    /// least one class. Trips when β.2's drop-at-emit filter regresses or a
    /// future refactor reintroduces the sparse-sidecar gap. Strict only for
    /// single-layer-per-binding pages; shared-binding pages legitimately
    /// produce per-layer sparse sidecars and are exempt.
    #[error(
        "compile invariant: layer {layer} page {page} class slots {class} != geometry slots {geom} \
         (single-layer-per-binding; β.2 should have dropped unmatched rows)"
    )]
    ClassGeometryMismatch {
        /// Affected layer.
        layer: String,
        /// Affected page id.
        page: mars_types::PageId,
        /// Geometry slot count.
        geom: usize,
        /// Class-assignment slot count.
        class: usize,
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
    /// Per-binding cycle counter that drives the periodic reconciliation hook
    /// in [`Self::run_cycle_once`]. Single-instance leader-elected compiler
    /// means we can keep this in process; on leader handover the counter
    /// resets, which is intentional (a new leader runs a fresh
    /// reconciliation pass before drift accumulates).
    cycle_counter: tokio::sync::RwLock<HashMap<BindingId, u32>>,
}

impl Compiler {
    /// Build a `Compiler` from its ports and validated config.
    #[must_use]
    pub fn new(deps: Deps, config: Config) -> Self {
        Self {
            deps,
            config,
            cycle_counter: tokio::sync::RwLock::new(HashMap::new()),
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
        .with_filter(binding_plan.filter.clone());
        let started = std::time::Instant::now();
        tracing::info!(
            target: "mars_compiler::compile",
            binding = %binding_plan.binding_id,
            "compile.binding.start",
        );
        let mut session = deps.source.open_compile_session(&port_binding).await?;
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

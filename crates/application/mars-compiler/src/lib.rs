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

pub mod class_eval;
pub mod decimate;
pub mod external_sort;
pub mod hilbert;
pub mod incremental;
pub mod plan;
pub mod rebalance;
pub mod rebuild;
pub mod reconcile;
pub mod sidecar;
pub mod snapshot;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use mars_config::Config;
use mars_observability::Metrics;
use mars_source::{ChangeBatch, ChangeEvent, ChangeFeed, LeaderLock, LeaderLockGuard, Source};
use mars_store::{ManifestStore, ObjectStore, StoreError};
use mars_types::{BindingId, LevelMetadata, Manifest, PageEntry};
use tokio_util::sync::CancellationToken;

use crate::sidecar::SidecarReader;

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
    /// Bootstrap accumulated bytes exceeded the configured scratch budget.
    /// Bootstrap streams source rows through a bucketed external sort whose
    /// in-memory accumulator drains to per-bucket spill files when it crosses
    /// `bootstrap_working_set_bytes * bootstrap_spill_threshold_fraction`.
    /// This error fires when total spill bytes (or the in-memory accumulator
    /// in the rebuild path, which keeps the same shape but does not spill)
    /// cross `compiler.bootstrap_scratch_budget_bytes`. LAZARUS bailout 5:
    /// lift the budget, point the scratch dir at a larger volume, or split
    /// the binding.
    #[error(
        "bootstrap scratch budget exceeded: binding {binding} accumulated {observed_bytes} bytes \
         (budget {budget_bytes}). lift compiler.bootstrap_scratch_budget_bytes, point \
         bootstrap_scratch_dir at a larger volume, or split the binding."
    )]
    ScratchBudgetExceeded {
        /// Affected binding id.
        binding: String,
        /// Observed accumulated bytes at the point the budget was crossed.
        observed_bytes: u64,
        /// Configured scratch budget.
        budget_bytes: u64,
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
        let plan = plan::build_bootstrap_plan(&self.config)?;
        let prev_version = self.deps.manifest.current().await?.map_or(0, |m| m.version);
        let next_version = prev_version + 1;
        let working_set_bytes = self.config.compiler.bootstrap_working_set()?;
        let manifest = snapshot::run_snapshot(
            &self.deps,
            &plan,
            self.config.service.name.clone(),
            next_version,
            working_set_bytes,
        )
        .await?;
        let v = publish_with_retry(self.deps.manifest.as_ref(), &manifest, &self.deps.metrics, &shutdown).await?;
        tracing::info!(
            version = v,
            bindings = manifest.bindings.len(),
            pages = manifest.pages.len(),
            "compiler: snapshot manifest published"
        );
        Ok(v)
    }

    /// Apply one or more change batches as a single incremental cycle and
    /// publish the resulting v3 manifest. Returns the published version.
    /// The caller (typically [`Self::run`]) is responsible for sourcing
    /// `batches` from a [`mars_source::ChangeSubscription`] and acking
    /// downstream once this returns.
    ///
    /// LAZARUS Phase C.2.c: this is the cycle entry point for the
    /// page-keyed substrate.
    pub async fn run_cycle_once(&self, batches: Vec<ChangeBatch>) -> Result<u64, CompilerError> {
        let _guard = self.acquire_leader().await?;
        self.apply_cycle(batches, &CancellationToken::new()).await
    }

    async fn apply_cycle(&self, batches: Vec<ChangeBatch>, shutdown: &CancellationToken) -> Result<u64, CompilerError> {
        let prior = self
            .deps
            .manifest
            .current()
            .await?
            .ok_or(CompilerError::NoPriorManifest {
                context: "run_cycle_once",
            })?;
        let plan = plan::build_bootstrap_plan(&self.config)?;

        // mmap each binding's page-membership sidecar.
        let mut sidecar_bytes: HashMap<BindingId, bytes::Bytes> = HashMap::new();
        for binding_meta in &prior.bindings {
            if let Some(entry) = &binding_meta.page_membership_sidecar {
                let bytes = self.deps.store.get(&entry.key, entry.hash).await?;
                sidecar_bytes.insert(binding_meta.binding_id.clone(), bytes);
            }
        }
        let mut sidecars: HashMap<BindingId, SidecarReader<'_>> = HashMap::new();
        for (id, bytes) in &sidecar_bytes {
            let reader = SidecarReader::open(bytes)?;
            sidecars.insert(id.clone(), reader);
        }

        // periodic reconciliation: bump per-binding counters; for any binding
        // that hits its cadence, run a reconciliation pass and prepend the
        // synthetic events to the cycle. counters reset to zero on fire so
        // that a single oversized cycle doesn't repeatedly trigger.
        let mut reconcile_events: Vec<ChangeEvent> = Vec::new();
        {
            let mut counters = self.cycle_counter.write().await;
            for binding_plan in &plan.bindings {
                let counter = counters.entry(binding_plan.binding_id.clone()).or_insert(0);
                *counter = counter.saturating_add(1);
                if *counter >= binding_plan.reconcile_every_cycles {
                    *counter = 0;
                    if let Some(sc) = sidecars.get(&binding_plan.binding_id) {
                        let outcome = reconcile::reconcile_binding(&self.deps, binding_plan, sc).await?;
                        for w in [
                            ("missing_in_sidecar", outcome.report.missing_in_sidecar.len()),
                            ("orphan_in_sidecar", outcome.report.orphan_in_sidecar.len()),
                        ] {
                            if w.1 > 0 {
                                tracing::warn!(
                                    binding = binding_plan.binding_id.as_str(),
                                    kind = w.0,
                                    count = w.1,
                                    "page-membership sidecar drift repaired by reconciliation"
                                );
                            }
                        }
                        reconcile_events.extend(outcome.synthetic_events);
                    }
                }
            }
        }

        // build an incremental cycle, ingest every event.
        let level_meta: HashMap<BindingId, Vec<LevelMetadata>> = prior
            .bindings
            .iter()
            .map(|b| (b.binding_id.clone(), b.levels.clone()))
            .collect();
        let mut cycle = incremental::IncrementalCycle::new(&plan, &sidecars, &level_meta);
        let mut last_source_version: Option<String> = prior.source_version.clone();
        let mut event_count: u64 = 0;
        for event in reconcile_events {
            cycle.ingest(event)?;
            event_count += 1;
        }
        for batch in batches {
            for event in batch.events {
                cycle.ingest(event)?;
                event_count += 1;
            }
            if let Some(v) = batch.source_version {
                last_source_version = Some(v);
            }
        }
        let dirty = cycle.finish();
        for w in &dirty.warnings {
            tracing::warn!(?w, "incremental cycle warning");
        }
        self.deps.metrics.inc_compiler_dirty_cells(
            dirty
                .per_binding
                .values()
                .map(|d| d.per_level.values().map(|s| s.len() as u64).sum::<u64>())
                .sum::<u64>(),
        );
        if event_count > 0 {
            for _ in 0..event_count {
                self.deps.metrics.inc_compiler_change_events();
            }
        }
        if dirty.per_binding.is_empty() {
            // no work; publish a no-op version bump so downstream cursors
            // advance even on empty windows.
            let next_version = prior.version + 1;
            let mut next = prior.clone();
            next.version = next_version;
            next.epoch = next_version;
            next.source_version = last_source_version;
            next.created_at = std::time::SystemTime::now();
            return publish_with_retry(self.deps.manifest.as_ref(), &next, &self.deps.metrics, shutdown).await;
        }

        // rebuild dirty pages.
        let working_set_bytes = self.config.compiler.bootstrap_working_set()?;
        let started = std::time::Instant::now();
        let outcome = rebuild::rebuild_pages(&self.deps, &plan, &prior, &sidecars, dirty, working_set_bytes).await?;
        self.deps.metrics.observe_compiler_rebuild_duration(started.elapsed());

        // merge outcome into prior to produce the new manifest.
        let next_version = prior.version + 1;
        let new_manifest = merge_manifest(&prior, &outcome, next_version, last_source_version);
        publish_with_retry(self.deps.manifest.as_ref(), &new_manifest, &self.deps.metrics, shutdown).await
    }

    /// Run one opportunistic rebalance pass over the current manifest.
    /// Identifies pages outside the size band or with dilated bboxes via
    /// [`crate::rebalance::rebalance_candidates`] and rewrites them through
    /// [`crate::rebuild::execute_rebalance`]. No-op when the manifest is
    /// already balanced.
    ///
    /// LAZARUS Phase C.2.d. The daily rebalance window driven from
    /// [`Self::run`] is a follow-up; here we only expose the executor.
    pub async fn run_rebalance_once(&self) -> Result<u64, CompilerError> {
        let _guard = self.acquire_leader().await?;
        let prior = self
            .deps
            .manifest
            .current()
            .await?
            .ok_or(CompilerError::NoPriorManifest {
                context: "run_rebalance_once",
            })?;
        let plan = plan::build_bootstrap_plan(&self.config)?;

        // collect candidate ops across every (binding, level).
        let mut ops: Vec<rebalance::RebalanceOp> = Vec::new();
        for binding_meta in &prior.bindings {
            let Some(binding_plan) = plan.bindings.iter().find(|b| b.binding_id == binding_meta.binding_id) else {
                continue;
            };
            for level in &binding_meta.levels {
                let level_pages: Vec<PageEntry> = prior
                    .pages
                    .iter()
                    .filter(|p| p.key.binding_id == binding_meta.binding_id && p.key.level == level.level)
                    .cloned()
                    .collect();
                ops.extend(rebalance::rebalance_candidates(
                    level,
                    &level_pages,
                    binding_plan.page_size_target_bytes,
                ));
            }
        }
        if ops.is_empty() {
            // already balanced; bump version so cursors advance.
            let next_version = prior.version + 1;
            let mut next = prior.clone();
            next.version = next_version;
            next.epoch = next_version;
            next.created_at = std::time::SystemTime::now();
            return publish_with_retry(
                self.deps.manifest.as_ref(),
                &next,
                &self.deps.metrics,
                &CancellationToken::new(),
            )
            .await;
        }

        // mmap each binding's page-membership sidecar so the executor can
        // resolve feature-id sets per source page.
        let mut sidecar_bytes: HashMap<BindingId, bytes::Bytes> = HashMap::new();
        for binding_meta in &prior.bindings {
            if let Some(entry) = &binding_meta.page_membership_sidecar {
                let bytes = self.deps.store.get(&entry.key, entry.hash).await?;
                sidecar_bytes.insert(binding_meta.binding_id.clone(), bytes);
            }
        }
        let mut sidecars: HashMap<BindingId, SidecarReader<'_>> = HashMap::new();
        for (id, bytes) in &sidecar_bytes {
            let reader = SidecarReader::open(bytes)?;
            sidecars.insert(id.clone(), reader);
        }

        let working_set_bytes = self.config.compiler.bootstrap_working_set()?;
        let outcome = rebuild::execute_rebalance(&self.deps, &plan, &prior, &sidecars, ops, working_set_bytes).await?;
        let next_version = prior.version + 1;
        let new_manifest = merge_manifest(&prior, &outcome, next_version, prior.source_version.clone());
        publish_with_retry(
            self.deps.manifest.as_ref(),
            &new_manifest,
            &self.deps.metrics,
            &CancellationToken::new(),
        )
        .await
    }

    /// Long-running service mode. Bootstraps from an empty manifest, then
    /// drives one cycle per `compiler.window`. Phase C.2.c first cut: this
    /// is a one-shot orchestrator that runs to `shutdown`. Multi-batch
    /// window-driven pulling (see LAZARUS §Update semantics) lands in a
    /// follow-up commit; the per-cycle correctness lives entirely in
    /// [`Self::run_cycle_once`].
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

/// Merge a [`rebuild::RebuildOutcome`] into the prior manifest to produce
/// the next version. Pure; safe to test in isolation.
fn merge_manifest(
    prior: &Manifest,
    outcome: &rebuild::RebuildOutcome,
    next_version: u64,
    source_version: Option<String>,
) -> Manifest {
    let replacement_page_keys: std::collections::HashSet<mars_types::PageKey> =
        outcome.replacement_pages.iter().map(|p| p.key.clone()).collect();
    let dropped_page_keys: std::collections::HashSet<mars_types::PageKey> =
        outcome.dropped_pages.iter().cloned().collect();

    let replacement_class_keys: std::collections::HashSet<(mars_types::LayerId, mars_types::PageKey)> = outcome
        .replacement_class_sidecars
        .iter()
        .map(|s| (s.layer_id.clone(), s.page_key.clone()))
        .collect();
    let replacement_label_keys: std::collections::HashSet<(mars_types::LayerId, mars_types::PageKey)> = outcome
        .replacement_label_sidecars
        .iter()
        .map(|s| (s.layer_id.clone(), s.page_key.clone()))
        .collect();
    let dropped_class_keys: std::collections::HashSet<(mars_types::LayerId, mars_types::PageKey)> =
        outcome.dropped_class_sidecars.iter().cloned().collect();
    let dropped_label_keys: std::collections::HashSet<(mars_types::LayerId, mars_types::PageKey)> =
        outcome.dropped_label_sidecars.iter().cloned().collect();

    // pages: keep prior pages whose key isn't replaced/dropped, then append
    // replacements.
    let mut pages: Vec<PageEntry> = prior
        .pages
        .iter()
        .filter(|p| !replacement_page_keys.contains(&p.key) && !dropped_page_keys.contains(&p.key))
        .cloned()
        .collect();
    pages.extend(outcome.replacement_pages.iter().cloned());
    pages.sort_by(|a, b| {
        a.key
            .binding_id
            .as_str()
            .cmp(b.key.binding_id.as_str())
            .then_with(|| a.key.level.cmp(&b.key.level))
            .then_with(|| a.hilbert_range.0.cmp(&b.hilbert_range.0))
    });

    // class / label sidecars: same shape.
    let mut class_sidecars = prior
        .class_sidecars
        .iter()
        .filter(|s| {
            let k = (s.layer_id.clone(), s.page_key.clone());
            !replacement_class_keys.contains(&k) && !dropped_class_keys.contains(&k)
        })
        .cloned()
        .collect::<Vec<_>>();
    class_sidecars.extend(outcome.replacement_class_sidecars.iter().cloned());

    let mut label_sidecars = prior
        .label_sidecars
        .iter()
        .filter(|s| {
            let k = (s.layer_id.clone(), s.page_key.clone());
            !replacement_label_keys.contains(&k) && !dropped_label_keys.contains(&k)
        })
        .cloned()
        .collect::<Vec<_>>();
    label_sidecars.extend(outcome.replacement_label_sidecars.iter().cloned());

    // bindings: replace touched ones, then refresh hilbert_range_table per
    // level via rebuild::recompute_level_metadata.
    let refreshed_ids: std::collections::HashSet<BindingId> = outcome
        .refreshed_bindings
        .iter()
        .map(|b| b.binding_id.clone())
        .collect();
    let mut bindings: Vec<mars_types::BindingMetadata> = prior
        .bindings
        .iter()
        .filter(|b| !refreshed_ids.contains(&b.binding_id))
        .cloned()
        .collect();
    bindings.extend(outcome.refreshed_bindings.iter().cloned());
    for b in &mut bindings {
        for lm in &mut b.levels {
            *lm = rebuild::recompute_level_metadata(lm, &pages, &b.binding_id);
        }
    }
    bindings.sort_by(|a, b| a.binding_id.as_str().cmp(b.binding_id.as_str()));

    Manifest {
        format_version: mars_types::MANIFEST_FORMAT_VERSION,
        version: next_version,
        service: prior.service.clone(),
        created_at: std::time::SystemTime::now(),
        bindings,
        pages,
        class_sidecars,
        label_sidecars,
        style_artifact: prior.style_artifact.clone(),
        source_version,
        epoch: next_version,
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

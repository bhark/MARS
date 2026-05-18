//! crate-wide error enum. one variant per typed failure mode surfaced from
//! the compiler; `#[from]` plumbs adapter errors in without losing the
//! underlying source. only `NotLeader` is matched externally (by the
//! service bins); everything else propagates via `?`.

use crate::{incremental, plan, sidecar};

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
    /// A binding referenced a source id absent from the registry. Config
    /// validation should catch this before compile; surfaces here only when
    /// the registry was built without the declared source.
    #[error("binding {binding} references unknown source id {source_id}")]
    UnknownSource {
        /// Affected binding id.
        binding: String,
        /// The missing source id.
        source_id: String,
    },
    /// An incremental change event's hilbert key fell outside every page
    /// range and the binding's `MissingPagePolicy::Fail` policy elected to
    /// surface this as a cycle-failing error.
    #[error(
        "missing-page escalation: binding {binding} level {level} key {key:?} outside any page range; \
         hint: switch on_missing_page to `truncate` to auto-heal, or `warn` to defer to reconcile."
    )]
    MissingPageEscalation {
        /// Affected binding.
        binding: String,
        /// Affected decimation level.
        level: u8,
        /// The unresolved hilbert key (raw 64-bit value).
        key: u64,
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
    /// Bitmap pack assembly failed - either the configured `images_dir` is
    /// missing while a style references an image, the file for a named
    /// image is missing or unreadable, or the artifact codec rejected the
    /// pack.
    #[error("image_pack: {what}: {detail}")]
    ImagePack {
        /// Stable short label naming the failure (e.g. "images_dir missing",
        /// "image file read", "section encode").
        what: &'static str,
        /// Human-readable detail. For file errors this is the path; for
        /// codec errors this is the underlying message.
        detail: String,
    },
}

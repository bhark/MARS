//! mars compiler use-case. Phase 0 ships the snapshot path only: read config,
//! enumerate cells, fetch rows, build source + layer artifacts, publish a
//! manifest. The change-feed dependency is held for forward-compat (Phase 1).

#![forbid(unsafe_code)]

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::stream::{self, StreamExt};
use mars_config::Config;
use mars_observability::Metrics;
use mars_source::{ChangeFeed, LeaderLock, LeaderLockGuard, Source, SourceBinding};
use mars_store::{ManifestStore, ObjectStore, StoreError};
use mars_types::{Bbox, Manifest};
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

/// Fallback concurrency when neither config nor `available_parallelism` yield
/// a usable value. The connection pool size on the source side becomes the
/// real ceiling, but 16 is a reasonable starting point for typical pg pools.
const FALLBACK_SNAPSHOT_CONCURRENCY: usize = 16;

/// Cells per pipelined `Source::fetch_cells` call. Keeps the fetch chunk small
/// enough that each chunk only consumes one `parallel_cells` slot at a time —
/// one giant batch per binding-shape would collapse the whole budget down to
/// one connection. Promote to config if benches motivate it.
const FETCH_CHUNK: usize = 48;

/// Capped exponential backoff schedule for retrying a transient publish.
/// On exhaustion the underlying error propagates so the supervisor restarts.
const PUBLISH_RETRY_DELAYS: &[Duration] = &[
    Duration::from_millis(100),
    Duration::from_millis(500),
    Duration::from_secs(2),
    Duration::from_secs(8),
];

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
            publish_with_retry(self.deps.manifest.as_ref(), &merged, &self.deps.metrics, &shutdown).await?;
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
        let output = self.execute_plan(plan, shutdown.clone()).await?;

        let manifest = Manifest::new(
            1,
            self.config.service.name.clone(),
            output.source_artifacts,
            output.layer_artifacts,
            None,
            output.empty_layer_cells,
        );
        let v = publish_with_retry(self.deps.manifest.as_ref(), &manifest, &self.deps.metrics, &shutdown).await?;
        self.deps
            .metrics
            .observe_compiler_rebuild_duration(rebuild_start.elapsed());
        tracing::info!(version = v, "compiler: manifest published");
        Ok(manifest)
    }

    /// Drive one plan through the per-source-cell rebuild pipeline. Concurrency
    /// comes from `compiler.parallel_cells` if set, else `available_parallelism`,
    /// else [`FALLBACK_SNAPSHOT_CONCURRENCY`].
    ///
    /// SourceTasks are grouped by their canonicalised `SourceBinding` and each
    /// group is chunked to [`FETCH_CHUNK`] cells per `Source::fetch_cells`
    /// call. The PG adapter pipelines a chunk's queries on one connection,
    /// amortising RTT; the in-memory default loops `fetch_cell`. Per-chunk
    /// futures fan out via `buffer_unordered(parallel_cells)`.
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
        let units: Vec<SourceUnit> = sources
            .into_iter()
            .zip(dependents)
            .map(|(s, deps)| {
                let dep_arcs: Vec<Arc<plan::LayerTask>> = deps.into_iter().map(|i| layer_arcs[i].clone()).collect();
                (Arc::new(s), dep_arcs)
            })
            .collect();

        let chunks = group_and_chunk_units(units, FETCH_CHUNK);

        let parallel = self.snapshot_concurrency();
        let mut stream = stream::iter(chunks)
            .map(|chunk| {
                let source = source.clone();
                let store = store.clone();
                async move { run_chunk(chunk, &source, &store).await }
            })
            .buffer_unordered(parallel);
        while let Some(result) = stream.next().await {
            if shutdown.is_cancelled() {
                return Ok(output);
            }
            output.extend(result?);
        }
        Ok(output)
    }

    fn snapshot_concurrency(&self) -> usize {
        self.config
            .compiler
            .parallel_cells
            .map(NonZeroUsize::get)
            .or_else(|| std::thread::available_parallelism().ok().map(NonZeroUsize::get))
            .unwrap_or(FALLBACK_SNAPSHOT_CONCURRENCY)
    }
}

/// One source-cell unit: a `SourceTask` plus the layer tasks it feeds.
type SourceUnit = (Arc<plan::SourceTask>, Vec<Arc<plan::LayerTask>>);

/// One executor unit: every cell in `chunk` shares `binding`, so a single
/// `fetch_cells` call materialises every row and the per-cell builds run
/// against pre-fetched rows.
struct FetchChunk {
    binding: SourceBinding,
    units: Vec<SourceUnit>,
}

/// Group SourceTasks by their canonicalised `SourceBinding` (every field —
/// schema, table, geom column, id column, attribute projection, CRS,
/// collection name) and chunk each group to `chunk_size` cells per fetch.
/// Two layers can reference the same `SourceCollectionId` with disjoint
/// attribute unions or different schema/table; those produce different SQL
/// shapes and cannot share a fetch call.
fn group_and_chunk_units(units: Vec<SourceUnit>, chunk_size: usize) -> Vec<FetchChunk> {
    // typical configs have a handful of distinct bindings, so a linear-search
    // grouping is faster than building a hashmap here. SourceBinding has
    // `PartialEq + Eq` already.
    let mut groups: Vec<(SourceBinding, Vec<SourceUnit>)> = Vec::new();
    for unit in units {
        let key = &unit.0.binding;
        if let Some(g) = groups.iter_mut().find(|(b, _)| b == key) {
            g.1.push(unit);
        } else {
            groups.push((unit.0.binding.clone(), vec![unit]));
        }
    }

    let chunk_size = chunk_size.max(1);
    let mut out: Vec<FetchChunk> = Vec::new();
    for (binding, group_units) in groups {
        for chunk in group_units.chunks(chunk_size) {
            out.push(FetchChunk {
                binding: binding.clone(),
                units: chunk.to_vec(),
            });
        }
    }
    out
}

/// Fetch every cell in `chunk` through one `Source::fetch_cells` call, then
/// drive each cell's encode + publish from the pre-fetched rows. The
/// `fetch_cells` port contract preserves input order, so we route results by
/// position and assert the cell labels match for safety.
async fn run_chunk(
    chunk: FetchChunk,
    source: &Arc<dyn Source>,
    store: &Arc<dyn ObjectStore>,
) -> Result<snapshot::SnapshotOutput, CompilerError> {
    let FetchChunk { binding, units } = chunk;

    let cells: Vec<(mars_types::Cell, Bbox)> = units
        .iter()
        .map(|(task, _)| {
            let bbox = plan::cell_bbox(task.origin, task.cell_size, &task.cell);
            (task.cell.clone(), bbox)
        })
        .collect();

    let fetched = source.fetch_cells(&binding, &cells, None).await?;
    if fetched.len() != units.len() {
        return Err(CompilerError::Source(mars_source::SourceError::Backend(format!(
            "fetch_cells returned {} results for {} cells",
            fetched.len(),
            units.len(),
        ))));
    }

    let mut out = snapshot::SnapshotOutput::default();
    for ((task, deps), (cell, rows)) in units.into_iter().zip(fetched) {
        debug_assert_eq!(task.cell, cell, "fetch_cells must preserve input order");
        let result = snapshot::build_and_publish(&task, &deps, rows, store).await?;
        out.extend(result);
    }
    Ok(out)
}

/// Publish a manifest, retrying on transient `StoreError::Transient` with the
/// schedule in [`PUBLISH_RETRY_DELAYS`]. Terminal errors propagate immediately.
/// Honours `shutdown`: a cancellation during a backoff sleep aborts the retry
/// loop and returns the most recent transient error.
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_source::SourceCollectionId;
    use mars_types::{Cell, CrsCode, ScaleBand};

    fn binding(collection: &str, attrs: Vec<&str>) -> SourceBinding {
        SourceBinding::new(
            SourceCollectionId::new(collection),
            "public",
            "t",
            "geom",
            "gid",
            attrs.into_iter().map(String::from).collect(),
            CrsCode::new("EPSG:25832"),
        )
        .unwrap()
    }

    fn unit(binding: SourceBinding, x: i64) -> SourceUnit {
        let task = plan::SourceTask {
            band: ScaleBand::new("hi"),
            cell: Cell {
                band: ScaleBand::new("hi"),
                x,
                y: 0,
            },
            binding,
            cell_size: 256.0,
            origin: (0.0, 0.0),
        };
        (Arc::new(task), Vec::new())
    }

    #[test]
    fn grouping_collapses_identical_bindings() {
        let b1 = binding("c", vec!["name"]);
        let units = vec![unit(b1.clone(), 0), unit(b1.clone(), 1), unit(b1, 2)];
        let chunks = group_and_chunk_units(units, 48);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].units.len(), 3);
    }

    #[test]
    fn grouping_separates_distinct_bindings() {
        // same collection, different attribute projection — different SQL shape.
        let units = vec![
            unit(binding("c", vec!["name"]), 0),
            unit(binding("c", vec!["name", "kind"]), 1),
            unit(binding("c", vec!["name"]), 2),
        ];
        let chunks = group_and_chunk_units(units, 48);
        assert_eq!(chunks.len(), 2, "two distinct bindings must produce two chunks");
        let total: usize = chunks.iter().map(|c| c.units.len()).sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn chunk_size_caps_in_flight_cells() {
        let b = binding("c", vec![]);
        let units: Vec<_> = (0..100i64).map(|x| unit(b.clone(), x)).collect();
        let chunks = group_and_chunk_units(units, 48);
        // 100 cells, chunk_size=48 -> 48, 48, 4
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].units.len(), 48);
        assert_eq!(chunks[1].units.len(), 48);
        assert_eq!(chunks[2].units.len(), 4);
    }
}

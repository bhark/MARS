//! mars runtime use-case: per-request render pipeline. depends on the
//! `mars-render-port` *port*, never a renderer adapter; the bin chooses one.

#![forbid(unsafe_code)]

mod draw;
mod fetch;
pub mod key;
mod plan;
pub mod state;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use futures_util::{StreamExt, TryStreamExt, stream};
use mars_artifact::ArtifactReader;
use mars_observability::{Metrics, reject_reason};
use mars_render_port::{Canvas, Encoder, Renderer};
use mars_store::{LocalCache, ManifestStore, ObjectStore, StoreError};
use mars_style::Stylesheet;
use mars_types::{ArtifactEntry, ArtifactKey, Bbox, CrsCode, ImageFormat, LayerId};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio::time::timeout;

pub use plan::denom_from_plan;
pub use state::RuntimeState;

const WARM_CONCURRENCY: usize = 8;
const WARM_TIMEOUT: Duration = Duration::from_secs(10);

fn default_render_concurrency() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

/// hard limit on cells a single request may cover. prevents oom from
/// pathological bbox / tiny cell size combinations.
pub(crate) const MAX_CELLS_PER_REQUEST: usize = 1_000_000;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("runtime is not ready")]
    NotReady,
    #[error(transparent)]
    Store(#[from] mars_store::StoreError),
    #[error(transparent)]
    Render(#[from] mars_render_port::RenderError),
    #[error(transparent)]
    Encode(#[from] mars_render_port::EncodeError),
    #[error(transparent)]
    Artifact(#[from] mars_artifact::ArtifactError),
    #[error(transparent)]
    Grid(#[from] mars_grid::GridError),
    #[error(transparent)]
    Proj(#[from] mars_proj::ProjError),
    #[error("layer '{layer}' is not defined")]
    LayerNotDefined { layer: String },
    #[error("manifest entry missing for layer '{layer}' band '{band}' cell {cell:?}")]
    ManifestEntryMissing {
        layer: String,
        band: String,
        cell: (i64, i64),
    },
    #[error("source artifact missing for collection '{collection}' band '{band}' cell {cell:?}")]
    SourceMissing {
        collection: String,
        band: String,
        cell: (i64, i64),
    },
    #[error("malformed manifest key '{key}': {reason}")]
    BadKey { key: String, reason: String },
    #[error(transparent)]
    Config(#[from] mars_config::ConfigError),
    #[error("not implemented: {what}")]
    NotImplemented { what: &'static str },
}

/// All ports the runtime needs.
#[derive(Clone)]
pub struct Deps {
    pub store: Arc<dyn ObjectStore>,
    pub cache: Arc<dyn LocalCache>,
    pub renderer: Arc<dyn Renderer>,
    pub encoder: Arc<dyn Encoder>,
    pub metrics: Metrics,
}

/// The render plan as produced by the interface adapter (WMS / WMTS).
#[derive(Debug, Clone)]
pub struct RenderPlan {
    pub layers: Vec<LayerId>,
    pub bbox: Bbox,
    pub width: u32,
    pub height: u32,
    pub crs: CrsCode,
    pub format: ImageFormat,
}

pub struct Runtime {
    state: ArcSwapOption<RuntimeState>,
    deps: Deps,
    /// reason the most recent manifest swap was rejected, if any. exposed for
    /// the debug endpoint; cleared on a successful swap.
    last_reject_reason: ArcSwapOption<String>,
    render_sem: Arc<Semaphore>,
}

impl Runtime {
    /// Compose a runtime without an active manifest snapshot.
    #[must_use]
    pub fn empty(deps: Deps) -> Self {
        Self {
            state: ArcSwapOption::empty(),
            deps,
            last_reject_reason: ArcSwapOption::empty(),
            render_sem: Arc::new(Semaphore::new(default_render_concurrency())),
        }
    }

    /// Compose a runtime from a pre-built state snapshot and the dep set.
    #[must_use]
    pub fn from_state(state: Arc<RuntimeState>, deps: Deps) -> Self {
        Self {
            state: ArcSwapOption::from(Some(state)),
            deps,
            last_reject_reason: ArcSwapOption::empty(),
            render_sem: Arc::new(Semaphore::new(default_render_concurrency())),
        }
    }

    /// Snapshot of the most recent manifest reject reason. `None` when the
    /// last observed manifest was accepted (or no manifest has been seen).
    #[must_use]
    pub fn last_reject_reason(&self) -> Option<Arc<String>> {
        self.last_reject_reason.load_full()
    }

    fn record_reject(&self, reason: String) {
        self.last_reject_reason.store(Some(Arc::new(reason)));
    }

    fn clear_reject(&self) {
        self.last_reject_reason.store(None);
    }

    /// Returns true when an active manifest snapshot is loaded.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.state.load().is_some()
    }

    /// Load the active state snapshot.
    #[must_use]
    pub fn current_state(&self) -> Option<Arc<RuntimeState>> {
        self.state.load_full()
    }

    /// Atomically replace the active state snapshot.
    pub fn swap_state(&self, state: Arc<RuntimeState>) {
        self.state.store(Some(state));
    }

    /// Execute one render plan and return encoded image bytes.
    pub async fn render(&self, plan: &RenderPlan) -> Result<Vec<u8>, RuntimeError> {
        let state = self.current_state().ok_or(RuntimeError::NotReady)?;

        for layer in &plan.layers {
            if !state.layer_order.contains(layer) {
                return Err(RuntimeError::LayerNotDefined {
                    layer: layer.as_str().to_owned(),
                });
            }
        }

        // reproject the request bbox into canonical CRS for cell selection.
        // forward (canonical -> request) transformer is built later inside
        // spawn_blocking — Transformer is !Send and must live on one thread.
        let needs_reproject = plan.crs != state.canonical_crs;
        let canonical_bbox = if needs_reproject {
            let inverse = mars_proj::Transformer::new(&plan.crs, &state.canonical_crs)?;
            inverse.transform_bbox(plan.bbox)?
        } else {
            plan.bbox
        };

        let tasks = plan::resolve(plan, &state, canonical_bbox)?;
        let viewport = draw::Viewport {
            bbox: plan.bbox,
            width: plan.width,
            height: plan.height,
        };

        // collect fetched artifacts under async; geometry + render run sync.
        let cache = self.deps.cache.clone();
        let store = self.deps.store.clone();
        let fetched: Vec<(ArtifactReader, ArtifactReader)> = stream::iter(
            tasks.into_iter().map(|task| {
                let state = state.clone();
                let cache = cache.clone();
                let store = store.clone();
                async move {
                    let layer_art = match fetch::fetch_layer(
                        &state,
                        cache.as_ref(),
                        store.as_ref(),
                        &task.layer,
                        &task.cell,
                    )
                    .await?
                    {
                        Some(reader) => reader,
                        None => return Ok(None),
                    };
                    let source_ref = layer_art.source_ref().cloned().ok_or_else(|| {
                        RuntimeError::Config(mars_config::ConfigError::Invalid(format!(
                            "layer artifact '{}' is missing source_ref footer",
                            task.layer
                        )))
                    })?;
                    let source_cell = mars_types::Cell {
                        band: mars_types::ScaleBand::new(source_ref.band.clone()),
                        x: source_ref.cell_x,
                        y: source_ref.cell_y,
                    };
                    let source_art = fetch::fetch_source(
                        &state,
                        cache.as_ref(),
                        store.as_ref(),
                        &source_ref.collection,
                        &source_cell,
                    )
                    .await?;
                    Ok::<_, RuntimeError>(Some((source_art, layer_art)))
                }
            }),
        )
        .buffer_unordered(8)
        .try_collect::<Vec<_>>()
        .await?
        .into_iter()
        .flatten()
        .collect();

        let canvas = Canvas {
            width: plan.width,
            height: plan.height,
            background: None,
        };
        let renderer = self.deps.renderer.clone();
        let encoder = self.deps.encoder.clone();
        let format = plan.format;
        let permit = self.render_sem.clone().acquire_owned().await.map_err(|_| {
            RuntimeError::Render(mars_render_port::RenderError::Backend("render semaphore closed".into()))
        })?;
        let canonical_crs = state.canonical_crs.clone();
        let request_crs = plan.crs.clone();
        let stylesheet_state = state.clone();
        let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, RuntimeError> {
            let _permit = permit;
            // build the forward transformer on the blocking thread so its
            // thread-local PJ context lives only here.
            let forward = if needs_reproject {
                Some(mars_proj::Transformer::new(&canonical_crs, &request_crs)?)
            } else {
                None
            };
            let mut ops = Vec::new();
            for (source_art, layer_art) in &fetched {
                draw::emit_layer_cell(
                    source_art,
                    layer_art,
                    &stylesheet_state.stylesheet,
                    viewport,
                    canonical_bbox,
                    forward.as_ref(),
                    &mut ops,
                )?;
            }
            let pixmap = renderer.render(canvas, &ops)?;
            Ok(encoder.encode(&pixmap, format)?)
        })
        .await
        .map_err(|e| {
            RuntimeError::Render(mars_render_port::RenderError::Backend(format!(
                "render task panicked: {e}"
            )))
        })??;
        Ok(bytes)
    }
}

/// Consume a manifest watch stream and atomically hot-swap valid runtime states.
pub async fn run_manifest_reload_loop(
    runtime: Arc<Runtime>,
    manifests: Arc<dyn ManifestStore>,
    config: Arc<mars_config::Config>,
    stylesheet: Stylesheet,
) -> Result<(), RuntimeError> {
    let mut manifests = manifests.watch().await?;
    let mut warming: Option<JoinHandle<()>> = None;

    while let Some(next) = manifests.next().await {
        let manifest = match next {
            Ok(manifest) => manifest,
            Err(e) => {
                let label = classify_store_error(&e);
                let reason = format!("invalid snapshot: {e}");
                tracing::error!(error = %e, "manifest watch: ignoring invalid snapshot");
                runtime.deps.metrics.inc_manifest_reject(label);
                runtime.record_reject(reason);
                continue;
            }
        };

        // monotonicity: refuse anything not strictly newer than the active
        // version. silent identical-version skip would mask a real downgrade.
        if let Some(current) = runtime.current_state() {
            if manifest.version == current.manifest.version {
                continue;
            }
            if manifest.version < current.manifest.version {
                let reason = format!(
                    "manifest version {} is older than active {}",
                    manifest.version, current.manifest.version
                );
                tracing::error!(
                    new_version = manifest.version,
                    current_version = current.manifest.version,
                    "manifest watch: rejecting older manifest"
                );
                runtime
                    .deps
                    .metrics
                    .inc_manifest_reject(reject_reason::BACKWARDS_VERSION);
                runtime.record_reject(reason);
                continue;
            }
        }

        let new_version = manifest.version;
        let state = match RuntimeState::from_config_and_manifest(&config, stylesheet.clone(), manifest) {
            Ok(state) => Arc::new(state),
            Err(e) => {
                let reason = format!("manifest v{new_version} rejected: {e}");
                tracing::error!(version = new_version, error = %e, "manifest watch: rejecting manifest");
                runtime
                    .deps
                    .metrics
                    .inc_manifest_reject(reject_reason::VALIDATION_ERROR);
                runtime.record_reject(reason);
                continue;
            }
        };

        let previous = runtime.current_state();
        let previous_keys = previous.as_deref().map(referenced_keys).unwrap_or_default();
        let warm_entries = referenced_entries(&state)
            .into_iter()
            .filter(|entry| !previous_keys.contains(&entry.key))
            .collect::<Vec<_>>();
        // eviction priority hint goes here when the runtime tracks per-key
        // refcounts; lru in the fs cache covers correctness today.

        runtime.swap_state(state);
        runtime.clear_reject();
        runtime.deps.metrics.set_manifest_version(new_version);

        if let Some(task) = warming.take() {
            task.abort();
            let _ = timeout(Duration::from_secs(30), task).await;
        }
        warming = Some(spawn_warming(
            runtime.deps.cache.clone(),
            runtime.deps.store.clone(),
            warm_entries,
        ));
    }

    Ok(())
}

/// classify a `StoreError` produced during manifest watch into a bounded
/// reject-reason label. anything not specifically recognised falls back to
/// `parse_error`.
fn classify_store_error(err: &StoreError) -> &'static str {
    match err {
        StoreError::UnsupportedManifestVersion { .. } => reject_reason::UNSUPPORTED_FORMAT_VERSION,
        StoreError::HashMismatch { .. } | StoreError::NotFound(_) => reject_reason::INVALID_SNAPSHOT,
        _ => reject_reason::PARSE_ERROR,
    }
}

fn referenced_entries(state: &RuntimeState) -> Vec<ArtifactEntry> {
    let present_count = state
        .layer_index
        .values()
        .filter(|s| matches!(s, state::LayerCellState::Present(_)))
        .count();
    let mut entries = Vec::with_capacity(
        present_count + state.source_index.len() + usize::from(state.manifest.style_artifact.is_some()),
    );
    entries.extend(state.layer_index.values().filter_map(|s| match s {
        state::LayerCellState::Present(e) => Some(e.clone()),
        state::LayerCellState::Empty => None,
    }));
    entries.extend(state.source_index.values().cloned());
    if let Some(entry) = &state.manifest.style_artifact {
        entries.push(entry.clone());
    }
    entries
}

/// keys referenced by `state`, collected directly without intermediate `Vec`.
/// hot path: every manifest swap calls this twice.
fn referenced_keys(state: &RuntimeState) -> HashSet<ArtifactKey> {
    let present_count = state
        .layer_index
        .values()
        .filter(|s| matches!(s, state::LayerCellState::Present(_)))
        .count();
    let mut out = HashSet::with_capacity(
        present_count + state.source_index.len() + usize::from(state.manifest.style_artifact.is_some()),
    );
    out.extend(state.layer_index.values().filter_map(|s| match s {
        state::LayerCellState::Present(e) => Some(e.key.clone()),
        state::LayerCellState::Empty => None,
    }));
    out.extend(state.source_index.values().map(|e| e.key.clone()));
    if let Some(entry) = &state.manifest.style_artifact {
        out.insert(entry.key.clone());
    }
    out
}

fn spawn_warming(
    cache: Arc<dyn LocalCache>,
    store: Arc<dyn ObjectStore>,
    entries: Vec<ArtifactEntry>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        stream::iter(entries)
            .for_each_concurrent(WARM_CONCURRENCY, |entry| {
                let cache = cache.clone();
                let store = store.clone();
                async move {
                    match timeout(WARM_TIMEOUT, cache.get_or_fetch(&entry.key, entry.hash, store.as_ref())).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => {
                            tracing::warn!(
                                key = %entry.key,
                                error = %e,
                                "manifest watch: artifact warm failed"
                            );
                        }
                        Err(_) => {
                            tracing::warn!(
                                key = %entry.key,
                                "manifest watch: artifact warm timed out"
                            );
                        }
                    }
                }
            })
            .await;
    })
}

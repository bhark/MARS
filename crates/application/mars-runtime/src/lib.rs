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
use futures_util::{StreamExt, stream};
use mars_render_port::{Canvas, Encoder, Renderer};
use mars_store::{LocalCache, ManifestStore, ObjectStore};
use mars_style::Stylesheet;
use mars_types::{ArtifactEntry, ArtifactKey, Bbox, CrsCode, ImageFormat, LayerId};
use tokio::task::JoinHandle;
use tokio::time::timeout;

pub use plan::denom_from_plan;
pub use state::RuntimeState;

const WARM_CONCURRENCY: usize = 8;

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
    #[error("plan CRS '{requested}' is not the canonical CRS; reprojection deferred to phase 1")]
    CrsNotCanonical { requested: String },
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
}

impl Runtime {
    /// Compose a runtime without an active manifest snapshot.
    #[must_use]
    pub fn empty(deps: Deps) -> Self {
        Self {
            state: ArcSwapOption::empty(),
            deps,
            last_reject_reason: ArcSwapOption::empty(),
        }
    }

    /// Compose a runtime from a pre-built state snapshot and the dep set.
    #[must_use]
    pub fn from_state(state: Arc<RuntimeState>, deps: Deps) -> Self {
        Self {
            state: ArcSwapOption::from(Some(state)),
            deps,
            last_reject_reason: ArcSwapOption::empty(),
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
        if plan.crs != state.canonical_crs {
            return Err(RuntimeError::CrsNotCanonical {
                requested: plan.crs.to_string(),
            });
        }

        let tasks = plan::resolve(plan, &state)?;
        let viewport = draw::Viewport {
            bbox: plan.bbox,
            width: plan.width,
            height: plan.height,
        };

        let mut ops = Vec::new();
        for task in &tasks {
            let layer_art = fetch::fetch_layer(
                &state,
                self.deps.cache.as_ref(),
                self.deps.store.as_ref(),
                &task.layer,
                &task.cell,
            )
            .await?;
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
                self.deps.cache.as_ref(),
                self.deps.store.as_ref(),
                &source_ref.collection,
                &source_cell,
            )
            .await?;

            draw::emit_layer_cell(&source_art, &layer_art, &state.stylesheet, viewport, &mut ops)?;
        }

        let canvas = Canvas {
            width: plan.width,
            height: plan.height,
            background: None,
        };
        let renderer = self.deps.renderer.clone();
        let encoder = self.deps.encoder.clone();
        let format = plan.format;
        // move ownership directly into the closure; the `ops` binding is dead
        // after this line, so peak memory is one Vec<DrawOp> not two.
        let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, RuntimeError> {
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
                let reason = format!("invalid snapshot: {e}");
                tracing::error!(error = %e, "manifest watch: ignoring invalid snapshot");
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

fn referenced_entries(state: &RuntimeState) -> Vec<ArtifactEntry> {
    let mut entries = Vec::with_capacity(
        state.layer_index.len() + state.source_index.len() + usize::from(state.manifest.style_artifact.is_some()),
    );
    entries.extend(state.layer_index.values().cloned());
    entries.extend(state.source_index.values().cloned());
    if let Some(entry) = &state.manifest.style_artifact {
        entries.push(entry.clone());
    }
    entries
}

/// keys referenced by `state`, collected directly without intermediate `Vec`.
/// hot path: every manifest swap calls this twice.
fn referenced_keys(state: &RuntimeState) -> HashSet<ArtifactKey> {
    let mut out = HashSet::with_capacity(
        state.layer_index.len() + state.source_index.len() + usize::from(state.manifest.style_artifact.is_some()),
    );
    out.extend(state.layer_index.values().map(|e| e.key.clone()));
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
                    if let Err(e) = cache.get_or_fetch(&entry.key, entry.hash, store.as_ref()).await {
                        tracing::warn!(
                            key = %entry.key,
                            error = %e,
                            "manifest watch: artifact warm failed"
                        );
                    }
                }
            })
            .await;
    })
}

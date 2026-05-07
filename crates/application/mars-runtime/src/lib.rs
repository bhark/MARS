//! mars runtime use-case: per-request render pipeline. depends on the
//! `mars-render-port` *port*, never a renderer adapter; the bin chooses one.

#![forbid(unsafe_code)]

mod draw;
mod fetch;
pub mod key;
mod labels;
mod plan;
pub mod state;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use futures_util::{StreamExt, TryStreamExt, stream};
use mars_artifact::ArtifactReader;
use mars_observability::{Metrics, reject_reason};
use mars_render_port::{Canvas, Encoder, Renderer};
use mars_store::{LocalCache, ManifestStore, ObjectStore, StoreError};
use mars_style::Stylesheet;
pub use mars_text::Fonts;
use mars_types::{ArtifactEntry, ArtifactKey, Bbox, CrsCode, ImageFormat, LayerId};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio::time::timeout;

pub use plan::denom_from_plan;
pub use state::RuntimeState;

const WARM_CONCURRENCY: usize = 8;
const WARM_TIMEOUT: Duration = Duration::from_secs(10);

/// default budget of in-flight raw-pixmap pixels across all concurrent renders.
/// 128M pixels ≈ 512 MiB at 4 bytes/pixel (premultiplied RGBA). a request
/// reserves `width * height` permits for the duration of its render, so this
/// caps total pixmap memory regardless of how many requests are dispatched.
const DEFAULT_PIXEL_BUDGET: u32 = 128_000_000;

/// hard limit on cells a single request may cover. prevents oom from
/// pathological bbox / tiny cell size combinations.
pub(crate) const MAX_CELLS_PER_REQUEST: usize = 1_000_000;

/// owned key for source-artifact dedup within a single render. layer artifacts'
/// `source_ref` carries owned strings so we can't borrow into the index without
/// extra plumbing; the cost is two heap clones per task, which is dwarfed by
/// the avoided artifact reopen.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct SourceKey {
    collection: String,
    band: String,
    x: i64,
    y: i64,
}

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
    #[error("request requires {requested} pixels but pixel_budget is {budget}")]
    PixelBudgetExceeded { requested: u64, budget: u32 },
}

/// All ports the runtime needs.
#[derive(Clone)]
pub struct Deps {
    pub store: Arc<dyn ObjectStore>,
    pub cache: Arc<dyn LocalCache>,
    pub renderer: Arc<dyn Renderer>,
    pub encoder: Arc<dyn Encoder>,
    pub metrics: Metrics,
    /// font registry shared across the label collision pass and the
    /// renderer adapter. cheap to clone behind `Arc`.
    pub fonts: Arc<Fonts>,
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
    /// pixel-budget semaphore. each render acquires `width * height` permits
    /// for the duration of its render, bounding total in-flight pixmap memory.
    render_sem: Arc<Semaphore>,
    /// pixel budget the semaphore was sized with. used to fail fast on
    /// requests that could never fit, instead of blocking forever.
    pixel_budget: u32,
}

impl Runtime {
    /// Compose a runtime without an active manifest snapshot.
    #[must_use]
    pub fn empty(deps: Deps) -> Self {
        Self::with_pixel_budget(deps, DEFAULT_PIXEL_BUDGET, None)
    }

    /// Compose a runtime from a pre-built state snapshot and the dep set.
    #[must_use]
    pub fn from_state(state: Arc<RuntimeState>, deps: Deps) -> Self {
        Self::with_pixel_budget(deps, DEFAULT_PIXEL_BUDGET, Some(state))
    }

    /// Compose a runtime with a custom pixel budget (raw pixmap pixels in flight).
    /// Used by the bin to thread the configured budget through.
    #[must_use]
    pub fn with_pixel_budget(deps: Deps, pixel_budget: u32, state: Option<Arc<RuntimeState>>) -> Self {
        Self {
            state: state.map_or_else(ArcSwapOption::empty, |s| ArcSwapOption::from(Some(s))),
            deps,
            last_reject_reason: ArcSwapOption::empty(),
            render_sem: Arc::new(Semaphore::new(pixel_budget as usize)),
            pixel_budget,
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

        // pixel-budget gate: fail fast if a single request exceeds the pool;
        // upstream parsers should already cap this, but a non-WMS plan source
        // (or a misconfigured budget) could otherwise wedge.
        let pixels = u64::from(plan.width) * u64::from(plan.height);
        if pixels > u64::from(self.pixel_budget) {
            return Err(RuntimeError::PixelBudgetExceeded {
                requested: pixels,
                budget: self.pixel_budget,
            });
        }

        // reproject the request bbox into canonical CRS for cell selection.
        // forward (canonical -> request) transformer is built later inside
        // spawn_blocking — Transformer is !Send and must live on one thread.
        // Both legs go through the per-thread cache so a busy worker amortises
        // proj_create_crs_to_crs + proj_normalize_for_visualization across
        // requests.
        let needs_reproject = plan.crs != state.canonical_crs;
        // both legs of reprojection go through the per-thread proj cache so a
        // busy worker amortises proj_create_crs_to_crs + normalize across
        // requests.
        let canonical_bbox = if needs_reproject {
            mars_proj::cached_transformer(&plan.crs, &state.canonical_crs)?.transform_bbox(plan.bbox)?
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
        // ordered `buffered` (not `buffer_unordered`) is required: tasks are
        // emitted in layer/cell z-order by `plan::resolve`, and the draw loop
        // below consumes them in vec order. completion-order delivery would
        // let a top-layer cell that finishes early sit beneath a later-finishing
        // bottom-layer cell, producing non-deterministic output.
        let cache = self.deps.cache.clone();
        let store = self.deps.store.clone();
        let layer_with_refs: Vec<(LayerId, ArtifactReader, SourceKey)> = stream::iter(tasks.into_iter().map(|task| {
            let state = state.clone();
            let cache = cache.clone();
            let store = store.clone();
            async move {
                let layer_art =
                    match fetch::fetch_layer(&state, cache.as_ref(), store.as_ref(), &task.layer, &task.cell).await? {
                        Some(reader) => reader,
                        None => return Ok(None),
                    };
                let source_ref = layer_art.source_ref().cloned().ok_or_else(|| {
                    RuntimeError::Config(mars_config::ConfigError::Invalid(format!(
                        "layer artifact '{}' is missing source_ref footer",
                        task.layer
                    )))
                })?;
                let key = SourceKey {
                    collection: source_ref.collection,
                    band: source_ref.band,
                    x: source_ref.cell_x,
                    y: source_ref.cell_y,
                };
                Ok::<_, RuntimeError>(Some((task.layer, layer_art, key)))
            }
        }))
        .buffered(8)
        .try_collect::<Vec<_>>()
        .await?
        .into_iter()
        .flatten()
        .collect();

        // dedupe source fetches: a single render often binds the same source
        // cell from several layers; opening the artifact twice would re-parse
        // the footer for no gain.
        let mut unique_keys: Vec<SourceKey> = Vec::new();
        let mut seen: HashSet<SourceKey> = HashSet::new();
        for (_, _, k) in &layer_with_refs {
            if seen.insert(k.clone()) {
                unique_keys.push(k.clone());
            }
        }
        let source_readers: Vec<(SourceKey, ArtifactReader)> = stream::iter(unique_keys.into_iter().map(|k| {
            let state = state.clone();
            let cache = cache.clone();
            let store = store.clone();
            async move {
                let cell = mars_types::Cell {
                    band: mars_types::ScaleBand::new(k.band.clone()),
                    x: k.x,
                    y: k.y,
                };
                let reader = fetch::fetch_source(&state, cache.as_ref(), store.as_ref(), &k.collection, &cell).await?;
                Ok::<_, RuntimeError>((k, reader))
            }
        }))
        .buffer_unordered(8)
        .try_collect::<Vec<_>>()
        .await?;
        let source_by_key: HashMap<SourceKey, ArtifactReader> = source_readers.into_iter().collect();

        let fetched: Vec<(LayerId, ArtifactReader, ArtifactReader)> = layer_with_refs
            .into_iter()
            .map(|(layer, layer_art, key)| {
                source_by_key
                    .get(&key)
                    .cloned()
                    .map(|source_art| (layer, source_art, layer_art))
                    .ok_or_else(|| {
                        RuntimeError::Render(mars_render_port::RenderError::Backend(
                            "internal: source artifact fetch table missing a key".into(),
                        ))
                    })
            })
            .collect::<Result<_, RuntimeError>>()?;

        let canvas = Canvas {
            width: plan.width,
            height: plan.height,
            background: None,
        };
        let renderer = self.deps.renderer.clone();
        let encoder = self.deps.encoder.clone();
        let format = plan.format;
        // safe: pixels <= pixel_budget (validated above), and pixel_budget fits u32.
        #[allow(clippy::cast_possible_truncation)]
        let permits = pixels as u32;
        let permit = self.render_sem.clone().acquire_many_owned(permits).await.map_err(|_| {
            RuntimeError::Render(mars_render_port::RenderError::Backend("render semaphore closed".into()))
        })?;
        let canonical_crs = state.canonical_crs.clone();
        let request_crs = plan.crs.clone();
        let stylesheet_state = state.clone();
        let fonts = self.deps.fonts.clone();
        let metrics = self.deps.metrics.clone();
        let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, RuntimeError> {
            let _permit = permit;
            // build the forward transformer through the per-thread cache so
            // back-to-back requests on the same blocking worker reuse it.
            let forward = if needs_reproject {
                Some(mars_proj::cached_transformer(&canonical_crs, &request_crs)?)
            } else {
                None
            };
            let mut ops = Vec::new();
            for (_, source_art, layer_art) in &fetched {
                draw::emit_layer_cell(
                    source_art,
                    layer_art,
                    &stylesheet_state.stylesheet,
                    viewport,
                    canonical_bbox,
                    forward.as_deref(),
                    &mut ops,
                )?;
            }
            // collect (layer, layer_art) pairs for the label pass. one rtree
            // is shared across all layers so labels collide globally.
            let label_layers: Vec<(LayerId, ArtifactReader)> = fetched
                .iter()
                .map(|(layer, _, layer_art)| (layer.clone(), layer_art.clone()))
                .collect();
            labels::collide_and_emit(
                &labels::LabelInputs {
                    layers: &label_layers,
                    stylesheet: &stylesheet_state.stylesheet,
                    viewport,
                    canonical_bbox,
                    reproject: forward.as_deref(),
                    fonts: &fonts,
                    metrics: &metrics,
                },
                &mut ops,
            )?;
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
/// Returns when the stream ends or `shutdown` is cancelled. On cancellation any
/// in-flight warming task is aborted and drained with a 30 s timeout.
pub async fn run_manifest_reload_loop(
    runtime: Arc<Runtime>,
    manifests: Arc<dyn ManifestStore>,
    config: Arc<mars_config::Config>,
    stylesheet: Stylesheet,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<(), RuntimeError> {
    let mut manifests = manifests.watch().await?;
    let mut warming: Option<JoinHandle<()>> = None;

    loop {
        let next = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                if let Some(task) = warming.take() {
                    task.abort();
                    let _ = timeout(Duration::from_secs(30), task).await;
                }
                return Ok(());
            }
            n = manifests.next() => match n {
                Some(n) => n,
                None => {
                    if let Some(task) = warming.take() {
                        task.abort();
                        let _ = timeout(Duration::from_secs(30), task).await;
                    }
                    return Ok(());
                }
            },
        };
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use bytes::Bytes;
    use mars_artifact::{ArtifactKind, ArtifactWriter, FeatureGeom, GeomKind, SourceRef};
    use mars_config::{
        ArtifactCache, ArtifactStore, Artifacts, Band, Cells, Class, ClassStyle, Config, Layer, Scales, ServiceMeta,
        Source,
    };
    use mars_render_port::{Canvas, DrawOp, EncodeError, Encoder, ImageFormat, Pixmap, RenderError, Renderer};
    use mars_store::mem::{InMemoryCache, InMemoryStore};
    use mars_store::{LocalCache, ObjectStore, StoreError};
    use mars_style::{Colour, Style, Stylesheet};
    use mars_types::{ArtifactEntry, ArtifactKey, Bbox, Cell, ContentHash, CrsCode, LayerId, Manifest, ScaleBand};
    use tokio::time::sleep;

    use crate::{Deps, RenderPlan, Runtime, RuntimeState, key};

    #[derive(Debug, Default)]
    struct RecordingRenderer {
        ops: Mutex<Vec<DrawOp>>,
    }

    impl Renderer for RecordingRenderer {
        fn render(&self, canvas: Canvas, ops: &[DrawOp]) -> Result<Pixmap, RenderError> {
            self.ops.lock().expect("poison").extend_from_slice(ops);
            Ok(Pixmap {
                width: canvas.width,
                height: canvas.height,
                premultiplied_rgba: vec![0; (canvas.width * canvas.height * 4) as usize],
            })
        }
    }

    #[derive(Debug, Default)]
    struct NoopEncoder;

    impl Encoder for NoopEncoder {
        fn encode(&self, _pixmap: &Pixmap, _format: ImageFormat) -> Result<Vec<u8>, EncodeError> {
            Ok(vec![])
        }
    }

    struct DelayingStore {
        delays: HashMap<ArtifactKey, Duration>,
        inner: InMemoryStore,
    }

    #[async_trait]
    impl ObjectStore for DelayingStore {
        async fn get(&self, key: &ArtifactKey, expected: ContentHash) -> Result<Bytes, StoreError> {
            if let Some(delay) = self.delays.get(key) {
                sleep(*delay).await;
            }
            self.inner.get(key, expected).await
        }
        async fn put(&self, key: &ArtifactKey, body: Bytes) -> Result<ContentHash, StoreError> {
            self.inner.put(key, body).await
        }
        async fn delete(&self, key: &ArtifactKey) -> Result<(), StoreError> {
            self.inner.delete(key).await
        }
        async fn list(&self, prefix: &str) -> Result<Vec<ArtifactKey>, StoreError> {
            self.inner.list(prefix).await
        }
    }

    fn source_artifact() -> Bytes {
        let mut w = ArtifactWriter::new(ArtifactKind::Source);
        w.add_geometry_payload(vec![FeatureGeom {
            id: 1,
            bbox: [0.0, 0.0, 1.0, 1.0],
            geom: GeomKind::Point((5.0, 5.0)),
        }])
        .set_bbox(Bbox::new(0.0, 0.0, 10.0, 10.0))
        .set_feature_count(1);
        w.finish().unwrap()
    }

    fn layer_artifact(collection: &str) -> Bytes {
        let mut w = ArtifactWriter::new(ArtifactKind::Layer);
        w.add_class_assignment(&[(1, 0)])
            .add_style_refs(&["red".into()])
            .set_source_ref(SourceRef {
                collection: collection.into(),
                band: "hi".into(),
                cell_x: 0,
                cell_y: 0,
                content_hash: ContentHash::zero(),
            })
            .set_bbox(Bbox::new(0.0, 0.0, 10.0, 10.0));
        w.finish().unwrap()
    }

    fn minimal_config() -> Config {
        let mut size_per_band = std::collections::BTreeMap::new();
        size_per_band.insert("hi".into(), "4096m".into());
        Config {
            service: ServiceMeta {
                name: "t".into(),
                ..Default::default()
            },
            source: Source {
                kind: "memory".into(),
                dsn: "memory://".into(),
                native_crs: CrsCode::new("EPSG:25832"),
                change_feed: None,
                pool: Default::default(),
            },
            artifacts: Artifacts {
                store: ArtifactStore {
                    kind: "fs".into(),
                    endpoint: None,
                    bucket: None,
                    prefix: None,
                    path: Some("/tmp".into()),
                },
                cache: ArtifactCache {
                    path: "/tmp".into(),
                    max_size: "1GiB".into(),
                    eviction: "lru".into(),
                    trust_path_hash: false,
                },
            },
            scales: Scales {
                bands: vec![Band {
                    name: "hi".into(),
                    max_denom: 25000,
                }],
            },
            cells: Cells {
                grid: "regular".into(),
                origin: [0.0, 0.0],
                size_per_band,
                extent: None,
            },
            interfaces: Default::default(),
            tile_matrix_sets: Default::default(),
            reprojection: Default::default(),
            styles: Default::default(),
            layers: vec![],
            observability: Default::default(),
            render: Default::default(),
            compiler: Default::default(),
        }
    }

    async fn build_state_with_two_layers(
        store: &InMemoryStore,
    ) -> (RuntimeState, Vec<ArtifactEntry>, Vec<ArtifactEntry>) {
        let cell = Cell {
            band: ScaleBand::new("hi"),
            x: 0,
            y: 0,
        };

        let layer_a_key = key::layer_key(&LayerId::new("layer_a"), &cell, "a");
        let layer_b_key = key::layer_key(&LayerId::new("layer_b"), &cell, "b");
        let source_a_key = key::source_key("src_a", &cell, "sa");
        let source_b_key = key::source_key("src_b", &cell, "sb");

        let hash_a = store.put(&layer_a_key, layer_artifact("src_a")).await.unwrap();
        let hash_b = store.put(&layer_b_key, layer_artifact("src_b")).await.unwrap();
        let hash_sa = store.put(&source_a_key, source_artifact()).await.unwrap();
        let hash_sb = store.put(&source_b_key, source_artifact()).await.unwrap();

        let layer_entries = vec![
            ArtifactEntry {
                key: layer_a_key,
                hash: hash_a,
                size_bytes: 0,
            },
            ArtifactEntry {
                key: layer_b_key,
                hash: hash_b,
                size_bytes: 0,
            },
        ];
        let source_entries = vec![
            ArtifactEntry {
                key: source_a_key,
                hash: hash_sa,
                size_bytes: 0,
            },
            ArtifactEntry {
                key: source_b_key,
                hash: hash_sb,
                size_bytes: 0,
            },
        ];

        let mut config = minimal_config();
        config.layers = vec![
            Layer {
                name: LayerId::new("layer_a"),
                title: String::new(),
                abstract_: String::new(),
                kind: "point".into(),
                scale: None,
                group: None,
                enable_get_feature_info: false,
                bbox: None,
                sources: vec![mars_config::SourceBinding {
                    scale: None,
                    band: Some("hi".into()),
                    from: "src_a".into(),
                    geometry_column: "geom".into(),
                    id_column: None,
                    attributes: vec![],
                }],
                classes: vec![Class {
                    name: "default".into(),
                    title: String::new(),
                    when: None,
                    style: ClassStyle::Ref { name: "red".into() },
                }],
                label: None,
            },
            Layer {
                name: LayerId::new("layer_b"),
                title: String::new(),
                abstract_: String::new(),
                kind: "point".into(),
                scale: None,
                group: None,
                enable_get_feature_info: false,
                bbox: None,
                sources: vec![mars_config::SourceBinding {
                    scale: None,
                    band: Some("hi".into()),
                    from: "src_b".into(),
                    geometry_column: "geom".into(),
                    id_column: None,
                    attributes: vec![],
                }],
                classes: vec![Class {
                    name: "default".into(),
                    title: String::new(),
                    when: None,
                    style: ClassStyle::Ref { name: "red".into() },
                }],
                label: None,
            },
        ];

        let mut stylesheet = Stylesheet::default();
        stylesheet.geometry.insert(
            "red".into(),
            Arc::new(Style {
                fill: Some(Colour::rgb(255, 0, 0)),
                ..Default::default()
            }),
        );

        let manifest = Manifest::new(1, "t", source_entries.clone(), layer_entries.clone(), None, vec![]);

        let state = RuntimeState::from_config_and_manifest(&config, stylesheet, manifest).unwrap();
        (state, layer_entries, source_entries)
    }

    #[tokio::test]
    async fn render_preserves_plan_order_with_inverted_latencies() {
        let inner_store = InMemoryStore::new();
        let (state, layer_entries, _source_entries) = build_state_with_two_layers(&inner_store).await;

        // delay first layer (layer_a) longer than second (layer_b)
        let mut delays = HashMap::new();
        delays.insert(layer_entries[0].key.clone(), Duration::from_millis(100));
        delays.insert(layer_entries[1].key.clone(), Duration::from_millis(10));

        let store = Arc::new(DelayingStore {
            delays,
            inner: inner_store,
        });
        let cache: Arc<dyn LocalCache> = Arc::new(InMemoryCache::new());
        let renderer = Arc::new(RecordingRenderer::default());

        let runtime = Runtime::from_state(
            Arc::new(state),
            Deps {
                store,
                cache,
                renderer: renderer.clone(),
                encoder: Arc::new(NoopEncoder),
                metrics: mars_observability::Metrics::new().unwrap(),
                fonts: std::sync::Arc::new(crate::Fonts::with_default()),
            },
        );

        let plan = RenderPlan {
            layers: vec![LayerId::new("layer_a"), LayerId::new("layer_b")],
            bbox: Bbox::new(0.0, 0.0, 10.0, 10.0),
            width: 100,
            height: 100,
            crs: CrsCode::new("EPSG:25832"),
            format: ImageFormat::Png,
        };

        let _ = runtime.render(&plan).await.unwrap();

        let ops = renderer.ops.lock().unwrap().clone();
        // each layer emits one DrawOp::Path for its point feature
        assert_eq!(ops.len(), 2, "expected two draw ops");
        match &ops[0] {
            DrawOp::Path { path, .. } => {
                // layer_a point at (5,5) projected to pixel space
                assert_eq!(path.subpaths[0].points[0], (50.0, 50.0));
            }
            other => panic!("expected Path, got {other:?}"),
        }
        match &ops[1] {
            DrawOp::Path { path, .. } => {
                assert_eq!(path.subpaths[0].points[0], (50.0, 50.0));
            }
            other => panic!("expected Path, got {other:?}"),
        }
    }
}

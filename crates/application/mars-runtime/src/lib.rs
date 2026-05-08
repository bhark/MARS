//! mars runtime use-case: per-request render pipeline.
//!
//! Phase B (LAZARUS): the cell-keyed substrate is retired with the v3
//! manifest cut, and the page-keyed render path lands in Phase D. The
//! crate's public API surface stays in place so the bins, the WMS / WMTS
//! interfaces, and the HTTP layer keep compiling; `Runtime::render`
//! short-circuits to `RuntimeError::NotImplemented` and the manifest
//! reload loop simply mirrors empty `RuntimeState`s into the swap slot.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use futures_util::StreamExt;
use mars_observability::{Metrics, reject_reason};
use mars_render_port::{Encoder, Renderer};
use mars_store::{LocalCache, ManifestStore, ObjectStore, StoreError};
use mars_style::Stylesheet;
pub use mars_text::Fonts;
use mars_types::{Bbox, CrsCode, ImageFormat, LayerId};
use tokio::sync::Semaphore;

mod fetch;
mod plan;
mod state;

pub use fetch::{fetch_page, fetch_sidecar};
pub use plan::{pick_binding_and_level, reproject_viewport, resolve_pages};
pub use state::{IndexError, PageIndex, RuntimeState};

/// default budget of in-flight raw-pixmap pixels across all concurrent renders.
const DEFAULT_PIXEL_BUDGET: u32 = 128_000_000;

/// default decoded-geometry cache size when constructed without an explicit one.
const DEFAULT_DECODED_CACHE_BYTES: usize = 256 * 1024 * 1024;

/// Decoded-geometry LRU cache. Phase B is a sized counter only; phase-d
/// reintroduces the real per-page geometry cache and its eviction policy.
#[derive(Debug)]
pub struct DecodedGeometryCache {
    capacity_bytes: usize,
    current_bytes: std::sync::atomic::AtomicUsize,
}

impl DecodedGeometryCache {
    /// Build an empty cache sized to `capacity_bytes`.
    #[must_use]
    pub fn new(capacity_bytes: usize) -> Self {
        Self {
            capacity_bytes,
            current_bytes: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Bytes currently retained.
    #[must_use]
    pub fn current_bytes(&self) -> usize {
        self.current_bytes.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Configured capacity in bytes.
    #[must_use]
    pub fn capacity_bytes(&self) -> usize {
        self.capacity_bytes
    }

    /// Drop all retained decoded geometry.
    pub fn clear(&self) {
        self.current_bytes.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Errors surfaced from the runtime.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// No manifest snapshot is loaded yet.
    #[error("runtime is not ready")]
    NotReady,
    /// Manifest / object store error.
    #[error(transparent)]
    Store(#[from] mars_store::StoreError),
    /// Renderer adapter error.
    #[error(transparent)]
    Render(#[from] mars_render_port::RenderError),
    /// Encoder error.
    #[error(transparent)]
    Encode(#[from] mars_render_port::EncodeError),
    /// Configuration error.
    #[error(transparent)]
    Config(#[from] mars_config::ConfigError),
    /// Layer not declared in config.
    #[error("layer '{layer}' is not defined")]
    LayerNotDefined {
        /// Layer that the request asked for.
        layer: String,
    },
    /// Phase-D: page-keyed render pipeline is not yet implemented.
    #[error("not implemented: {what}")]
    NotImplemented {
        /// Stable label naming the missing surface.
        what: &'static str,
    },
    /// Pixel budget would be exceeded by this request.
    #[error("request requires {requested} pixels but pixel_budget is {budget}")]
    PixelBudgetExceeded {
        /// Pixels requested by this render.
        requested: u64,
        /// Configured upper bound.
        budget: u32,
    },
    /// Manifest violates a structural invariant the runtime relies on for
    /// hot-path correctness (e.g. unsorted `pages` vector, orphan sidecars).
    #[error("invalid manifest: {reason}")]
    InvalidManifest {
        /// Human-readable description of the violation.
        reason: String,
    },
    /// A configured layer has no source binding present in the loaded
    /// manifest. Indicates either a stale manifest or a config that diverged
    /// from the compiler's BindingPlan.
    #[error("layer '{layer}' does not match the loaded manifest: {reason}")]
    ConfigManifestMismatch {
        /// Layer name from the configuration.
        layer: String,
        /// Human-readable mismatch reason.
        reason: String,
    },
}

/// All ports the runtime needs.
#[derive(Clone)]
pub struct Deps {
    /// Object store.
    pub store: Arc<dyn ObjectStore>,
    /// Local SSD cache.
    pub cache: Arc<dyn LocalCache>,
    /// Renderer adapter.
    pub renderer: Arc<dyn Renderer>,
    /// Encoder adapter.
    pub encoder: Arc<dyn Encoder>,
    /// Metrics handle.
    pub metrics: Metrics,
    /// Font registry.
    pub fonts: Arc<Fonts>,
}

/// The render plan as produced by the interface adapter (WMS / WMTS).
#[derive(Debug, Clone)]
pub struct RenderPlan {
    /// Layers to render in declared order.
    pub layers: Vec<LayerId>,
    /// Viewport bbox in `crs` units.
    pub bbox: Bbox,
    /// Viewport width in pixels.
    pub width: u32,
    /// Viewport height in pixels.
    pub height: u32,
    /// Request CRS.
    pub crs: CrsCode,
    /// Output image format.
    pub format: ImageFormat,
}

/// The runtime service.
pub struct Runtime {
    state: ArcSwapOption<RuntimeState>,
    deps: Deps,
    last_reject_reason: ArcSwapOption<String>,
    render_sem: Arc<Semaphore>,
    pixel_budget: u32,
    decoded_cache: Arc<DecodedGeometryCache>,
    parallel_emit: mars_config::ParallelEmit,
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

    /// Compose a runtime with a custom pixel budget.
    #[must_use]
    pub fn with_pixel_budget(deps: Deps, pixel_budget: u32, state: Option<Arc<RuntimeState>>) -> Self {
        Self::with_caches(
            deps,
            pixel_budget,
            Arc::new(DecodedGeometryCache::new(DEFAULT_DECODED_CACHE_BYTES)),
            state,
        )
    }

    /// Compose a runtime with the full set of tunable caches.
    #[must_use]
    pub fn with_caches(
        deps: Deps,
        pixel_budget: u32,
        decoded_cache: Arc<DecodedGeometryCache>,
        state: Option<Arc<RuntimeState>>,
    ) -> Self {
        Self::with_full_config(
            deps,
            pixel_budget,
            decoded_cache,
            mars_config::ParallelEmit::default(),
            state,
        )
    }

    /// Compose a runtime with all tunables plumbed in.
    #[must_use]
    pub fn with_full_config(
        deps: Deps,
        pixel_budget: u32,
        decoded_cache: Arc<DecodedGeometryCache>,
        parallel_emit: mars_config::ParallelEmit,
        state: Option<Arc<RuntimeState>>,
    ) -> Self {
        Self {
            state: state.map_or_else(ArcSwapOption::empty, |s| ArcSwapOption::from(Some(s))),
            deps,
            last_reject_reason: ArcSwapOption::empty(),
            render_sem: Arc::new(Semaphore::new(pixel_budget as usize)),
            pixel_budget,
            decoded_cache,
            parallel_emit,
        }
    }

    /// Snapshot of the most recent manifest reject reason.
    #[must_use]
    pub fn last_reject_reason(&self) -> Option<Arc<String>> {
        self.last_reject_reason.load_full()
    }

    /// Bytes currently retained by the decoded-geometry cache.
    #[must_use]
    pub fn decoded_cache_bytes(&self) -> usize {
        self.decoded_cache.current_bytes()
    }

    /// Drop all retained decoded geometry.
    pub fn clear_decoded_cache(&self) {
        self.decoded_cache.clear();
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
        self.last_reject_reason.store(None);
    }

    /// Execute one render plan and return encoded image bytes.
    ///
    /// Phase B stub: the cell-keyed plan resolution + decoder + draw pipeline
    /// is retired; phase-d reintroduces the page-keyed render path. Until
    /// then this returns `RuntimeError::NotImplemented`.
    pub async fn render(&self, _plan: &RenderPlan) -> Result<Vec<u8>, RuntimeError> {
        let _state = self.current_state().ok_or(RuntimeError::NotReady)?;
        // touch fields to silence unused warnings until phase-d wires them up.
        let _ = (
            &self.deps,
            &self.render_sem,
            self.pixel_budget,
            &self.decoded_cache,
            &self.parallel_emit,
        );
        Err(RuntimeError::NotImplemented {
            what: "mars-runtime::render (phase-d)",
        })
    }

    fn record_reject(&self, reason: String) {
        self.last_reject_reason.store(Some(Arc::new(reason)));
    }
}

/// Compute the rendered image's denominator at the configured viewport.
/// Phase B keeps the helper exposed for the WMS / WMTS interface code; the
/// formula is pure and unaffected by the substrate cut.
#[must_use]
pub fn denom_from_plan(bbox_width: f64, width_px: u32) -> u32 {
    if !bbox_width.is_finite() || bbox_width <= 0.0 || width_px == 0 {
        return u32::MAX;
    }
    // 0.00028 m/pixel is the OGC reference at 90 dpi; phase-d will revisit
    // dpi when it integrates the configurable pixel-density knob.
    let denom = bbox_width / (f64::from(width_px) * 0.000_28);
    if !denom.is_finite() || denom < 0.0 || denom > f64::from(u32::MAX) {
        u32::MAX
    } else {
        denom as u32
    }
}

/// Consume a manifest watch stream and atomically hot-swap valid runtime states.
/// Returns when the stream ends or `shutdown` is cancelled.
pub async fn run_manifest_reload_loop(
    runtime: Arc<Runtime>,
    manifests: Arc<dyn ManifestStore>,
    config: Arc<mars_config::Config>,
    stylesheet: Stylesheet,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<(), RuntimeError> {
    let mut manifests = manifests.watch().await?;

    loop {
        let next = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(()),
            n = manifests.next() => match n {
                Some(n) => n,
                None => return Ok(()),
            },
        };
        let manifest = match next {
            Ok(m) => m,
            Err(e) => {
                let label = classify_store_error(&e);
                let reason = format!("invalid snapshot: {e}");
                tracing::error!(error = %e, "manifest watch: ignoring invalid snapshot");
                runtime.deps.metrics.inc_manifest_reject(label);
                runtime.record_reject(reason);
                continue;
            }
        };

        // monotonicity: refuse anything not strictly newer than the active version.
        if let Some(current) = runtime.current_state()
            && manifest.version <= current.manifest.version
        {
            if manifest.version < current.manifest.version {
                let reason = format!(
                    "manifest version {} is older than active {}",
                    manifest.version, current.manifest.version
                );
                runtime
                    .deps
                    .metrics
                    .inc_manifest_reject(reject_reason::BACKWARDS_VERSION);
                runtime.record_reject(reason);
            }
            continue;
        }

        let new_version = manifest.version;
        match RuntimeState::from_config_and_manifest(&config, stylesheet.clone(), manifest) {
            Ok(state) => {
                runtime.swap_state(Arc::new(state));
                tracing::info!(version = new_version, "runtime: manifest swapped");
            }
            Err(e) => {
                let reason = format!("manifest v{new_version} rejected: {e}");
                runtime
                    .deps
                    .metrics
                    .inc_manifest_reject(reject_reason::VALIDATION_ERROR);
                runtime.record_reject(reason);
            }
        }
    }
}

fn classify_store_error(e: &StoreError) -> &'static str {
    match e {
        StoreError::UnsupportedManifestVersion { .. } => reject_reason::UNSUPPORTED_FORMAT_VERSION,
        StoreError::HashMismatch { .. } => reject_reason::HASH_MISMATCH,
        _ => reject_reason::IO_ERROR,
    }
}

// brief idle to keep tokio in scope for the runtime-reload loop helpers
// future-proofing — phase-d adds warm-allowlist prefetches that need a
// timeout window. exposed as a const so tests can match.
#[doc(hidden)]
pub const PHASE_B_IDLE_HINT: Duration = Duration::from_secs(5);

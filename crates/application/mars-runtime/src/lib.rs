//! mars runtime use-case: per-request render pipeline.
//!
//! Renders WMS / WMTS responses directly from versioned page artifacts
//! resolved through the active manifest. The reload loop polls the
//! manifest store and atomically swaps a fresh `RuntimeState` (manifest +
//! derived `PageIndex` + stylesheet) into a lock-free slot; render and
//! GFI use whatever snapshot was current when the request arrived.

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
mod gfi;
mod plan;
mod render;
mod state;

pub use fetch::{fetch_page, fetch_sidecar};
pub use gfi::LayerFeatureInfo;
pub use plan::{pick_binding_and_level, reproject_viewport, resolve_pages};
pub use state::{IndexError, PageIndex, RuntimeState};

/// default budget of in-flight raw-pixmap pixels across all concurrent renders.
const DEFAULT_PIXEL_BUDGET: u32 = 128_000_000;

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
    /// A surface the runtime exposes but does not yet implement.
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
    /// A class assignment named a stylesheet entry that the runtime's
    /// stylesheet does not contain. Surfaces drift between the compiler's
    /// emitted style refs and the runtime's loaded stylesheet.
    #[error("stylesheet entry '{name}' referenced by layer '{layer}' is missing")]
    StylesheetDrift {
        /// Layer that referenced the missing entry.
        layer: String,
        /// Stylesheet entry name that was not found.
        name: String,
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
    /// Standardised pixel size in metres used to compute the scale
    /// denominator from `(bbox, width)`. WMS adapters source this from
    /// `service.scale_dpi` (default 96 dpi → ≈ 0.0002645833 m/pixel),
    /// optionally overridden per-request by `&DPI=`. WMTS adapters set
    /// it to the OGC reference [`OGC_STANDARDIZED_PIXEL_SIZE_M`] because
    /// TileMatrixSet scale denominators are spec-fixed at that value.
    pub scale_pixel_size_m: f64,
}

/// OGC reference standardised pixel size: 0.28 mm = 90.7142857 dpi. WMTS
/// requires this exactly; WMS uses [`ServiceMeta::scale_pixel_size_m`].
///
/// [`ServiceMeta::scale_pixel_size_m`]: mars_config::ServiceMeta::scale_pixel_size_m
pub const OGC_STANDARDIZED_PIXEL_SIZE_M: f64 = 0.000_28;

/// The runtime service.
pub struct Runtime {
    state: ArcSwapOption<RuntimeState>,
    deps: Deps,
    last_reject_reason: ArcSwapOption<String>,
    render_sem: Arc<Semaphore>,
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

    /// Compose a runtime with a custom pixel budget.
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

    /// Snapshot of the most recent manifest reject reason.
    #[must_use]
    pub fn last_reject_reason(&self) -> Option<Arc<String>> {
        self.last_reject_reason.load_full()
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

    /// Resolve a pixel-space click into the matching `(layer, feature)` set
    /// for layers with `enable_get_feature_info`. The point is in render-plan
    /// pixel coordinates; out-of-bounds clicks return an empty list.
    pub async fn get_feature_info(
        &self,
        plan: &RenderPlan,
        point_px: (u32, u32),
    ) -> Result<Vec<LayerFeatureInfo>, RuntimeError> {
        let state = self.current_state().ok_or(RuntimeError::NotReady)?;
        gfi::get_feature_info(&state, &self.deps, plan, point_px).await
    }

    /// Execute one render plan and return encoded image bytes.
    pub async fn render(&self, plan: &RenderPlan) -> Result<Vec<u8>, RuntimeError> {
        let state = self.current_state().ok_or(RuntimeError::NotReady)?;
        let pixels = u64::from(plan.width).saturating_mul(u64::from(plan.height));
        if pixels > u64::from(self.pixel_budget) {
            return Err(RuntimeError::PixelBudgetExceeded {
                requested: pixels,
                budget: self.pixel_budget,
            });
        }
        // gate concurrent renders against the configured pixel budget. permits
        // are u32; we already verified pixels fits below.
        let permits = u32::try_from(pixels).map_err(|_| RuntimeError::PixelBudgetExceeded {
            requested: pixels,
            budget: self.pixel_budget,
        })?;
        let _permit = self.render_sem.acquire_many(permits).await.map_err(|_| {
            RuntimeError::Render(mars_render_port::RenderError::Backend("render semaphore closed".into()))
        })?;
        render::render_plan(&state, &self.deps, plan).await
    }

    fn record_reject(&self, reason: String) {
        self.last_reject_reason.store(Some(Arc::new(reason)));
    }
}

/// Compute the rendered image's denominator at the configured viewport.
/// Pure helper; exposed so the WMS / WMTS interface code can resolve
/// `<scaleHint>` style decisions without going through `Runtime`.
///
/// `m_per_pixel` is the standardised pixel size used to interpret the
/// denominator. Use [`OGC_STANDARDIZED_PIXEL_SIZE_M`] for OGC-pure
/// behaviour; pass the value derived from `service.scale_dpi` for parity
/// with deployments that pin a different DPI (typically 96).
#[must_use]
pub fn denom_from_plan(bbox_width: f64, width_px: u32, m_per_pixel: f64) -> u32 {
    if !bbox_width.is_finite() || bbox_width <= 0.0 || width_px == 0 || !m_per_pixel.is_finite() || m_per_pixel <= 0.0 {
        return u32::MAX;
    }
    let denom = bbox_width / (f64::from(width_px) * m_per_pixel);
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

// idle hint for reload-loop helpers; exposed so tests can match.
#[doc(hidden)]
pub const RELOAD_IDLE_HINT: Duration = Duration::from_secs(5);

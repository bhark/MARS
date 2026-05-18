//! runtime-facing error type. groups foreign errors (`mars_store`,
//! `mars_render_port`, `mars_config`, `mars_source`) under `#[from]`
//! conversions alongside structural variants the runtime emits itself
//! (manifest drift, pixel-budget overruns, unregistered raster
//! collections, etc.).

use mars_types::SourceCollectionId;

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
    /// A raster layer's manifest entry referenced a `collection` the bin
    /// did not register a `RasterSource` adapter for. Surfaces drift between
    /// the manifest and the composition wiring.
    #[error("raster source not registered for collection '{collection}'")]
    RasterSourceNotRegistered {
        /// Collection id named by the raster layer entry.
        collection: SourceCollectionId,
    },
    /// Underlying raster source failed.
    #[error(transparent)]
    RasterSource(#[from] mars_source::SourceError),
}

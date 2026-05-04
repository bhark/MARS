//! renderer port. tiny abstract vocabulary so the application layer can swap a
//! CPU rasteriser for a GPU one without touching the request path (SPEC §11.2).
//! the trait deliberately speaks in pixmaps, paths, paints; it does not name
//! `tiny-skia` types. concrete impls live in `mars-render`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use async_trait::async_trait;
use mars_style::Style;

pub use mars_types::ImageFormat;

/// Errors produced by the renderer.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// Adapter has not implemented this method yet.
    #[error("not implemented: {what}")]
    NotImplemented {
        /// Human-readable name of the unimplemented operation.
        what: &'static str,
    },
    /// Backend rasterisation error.
    #[error("backend error: {0}")]
    Backend(String),
    /// Encoder error (PNG / JPEG / ...).
    #[error("encode error: {0}")]
    Encode(String),
}

/// 2D path. Coordinates are in render-target pixel space; the application
/// crate handles vector reprojection and scaling before constructing draw ops.
#[derive(Debug, Clone)]
pub struct Path {
    /// Sequence of subpath rings; each ring is a polyline of `(x, y)` points.
    pub rings: Vec<Vec<(f32, f32)>>,
}

/// One draw operation. Intentionally narrow - adding shapes goes through this enum.
#[derive(Debug, Clone)]
pub enum DrawOp {
    /// Fill or stroke a path with the given style.
    Path {
        /// Geometry to draw.
        path: Path,
        /// Fill / stroke style.
        style: Style,
    },
    /// Place a label glyph run at an anchor point with the given style ref.
    Label {
        /// Anchor in pixel space.
        anchor: (f32, f32),
        /// Already-shaped text content.
        text: String,
        /// Reference to a compiled label style.
        style_ref: String,
    },
}

/// A render target: pixel dimensions plus background fill.
#[derive(Debug, Clone, Copy)]
pub struct Canvas {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Background fill (transparent black if `None`).
    pub background: Option<mars_style::Colour>,
}

/// Renderer port. Implementations may keep internal scratch buffers across
/// calls and must remain `Send + Sync` for use from the runtime task pool.
#[async_trait]
pub trait Renderer: Send + Sync + 'static {
    /// Rasterise `ops` onto `canvas` and encode to `format`. Returns the
    /// encoded image bytes.
    async fn render(&self, canvas: Canvas, ops: &[DrawOp], format: ImageFormat) -> Result<Vec<u8>, RenderError>;
}

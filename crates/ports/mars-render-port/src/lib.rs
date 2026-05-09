//! renderer port. tiny abstract vocabulary so the application layer can swap a
//! CPU rasteriser for a GPU one without touching the request path (SPEC §11.2).
//! the trait deliberately speaks in pixmaps, paths, paints; it does not name
//! `tiny-skia` types. concrete impls live in `mars-render`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::sync::Arc;

use mars_style::{LabelStyle, Style};

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
}

/// Errors produced by the encoder.
#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    /// Adapter does not yet support this image format.
    #[error("not implemented: {what}")]
    NotImplemented {
        /// Human-readable name of the unimplemented operation.
        what: &'static str,
    },
    /// Encoder backend failure (e.g. PNG / JPEG library error).
    #[error("encode error: {0}")]
    Backend(String),
}

/// One subpath within a [`Path`].
#[derive(Debug, Clone)]
pub struct Subpath {
    /// Sequence of `(x, y)` vertices in pixel space.
    pub points: Vec<(f32, f32)>,
    /// Whether the subpath is closed (polygon) or open (linestring).
    pub closed: bool,
}

/// 2D path. Coordinates are in render-target pixel space; the application
/// crate handles vector reprojection and scaling before constructing draw ops.
#[derive(Debug, Clone)]
pub struct Path {
    /// Sequence of subpaths. polygons are closed, linestrings are open.
    pub subpaths: Vec<Subpath>,
}

/// One draw operation. Intentionally narrow - adding shapes goes through this enum.
#[derive(Debug, Clone)]
pub enum DrawOp {
    /// Fill or stroke a path with the given style.
    Path {
        /// Geometry to draw.
        path: Path,
        /// Fill / stroke style.
        style: Arc<Style>,
    },
    /// Place a label glyph run at an anchor point with the given style.
    Label {
        /// Anchor in pixel space (baseline).
        anchor: (f32, f32),
        /// Plain-text content; renderer shapes and rasterises.
        text: String,
        /// Compiled label style.
        style: Arc<LabelStyle>,
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

/// Raw rasterised pixmap returned by [`Renderer::render`]. The buffer is
/// premultiplied 8-bit RGBA in row-major order (`width * height * 4` bytes).
/// Encoders are responsible for any colour-space conversion (e.g. PNG demul).
#[derive(Debug, Clone)]
pub struct Pixmap {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Premultiplied RGBA, row-major, 4 bytes per pixel.
    pub premultiplied_rgba: Vec<u8>,
}

/// Shaped-text metrics in pixel space. Returned by [`Renderer::measure_text`]
/// so the application layer can size collision bboxes against the same font
/// path the renderer will later use to rasterise the run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextMetrics {
    /// Horizontal advance of the shaped run in pixels (sum of glyph
    /// advances). Pre-cluster, post-shaping.
    pub advance_x: f32,
    /// Distance from baseline to the run's top in pixels (positive).
    pub ascent: f32,
    /// Distance from baseline to the run's bottom in pixels (positive).
    pub descent: f32,
}

/// Renderer port. Implementations may keep internal scratch buffers across
/// calls and must remain `Send + Sync` for use from the runtime task pool.
///
/// `render` is intentionally synchronous: rasterisation is cpu-bound work
/// that should run on a blocking thread pool, not the async executor.
pub trait Renderer: Send + Sync + 'static {
    /// Rasterise `ops` onto `canvas`. Returns the raw pixmap; encoding is
    /// the caller's responsibility (see [`Encoder`]).
    fn render(&self, canvas: Canvas, ops: &[DrawOp]) -> Result<Pixmap, RenderError>;

    /// Shape `text` under `style` and return font-aware pixel metrics. The
    /// label collision pass uses the result to size each candidate's bbox so
    /// it agrees with what `render` will later paint, avoiding the
    /// fudge-factor drift of a chars-times-font-size approximation.
    fn measure_text(&self, text: &str, style: &LabelStyle) -> Result<TextMetrics, RenderError>;
}

/// Encoder port. Splits image-format encoding from rasterisation so the two
/// concerns can evolve independently (e.g. swap PNG library, add JPEG).
pub trait Encoder: Send + Sync + 'static {
    /// Encode `pixmap` to `format`-specific bytes (PNG / JPEG / ...).
    fn encode(&self, pixmap: &Pixmap, format: ImageFormat) -> Result<Vec<u8>, EncodeError>;
}

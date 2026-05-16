//! renderer port. tiny abstract vocabulary so the application layer can swap a
//! CPU rasteriser for a GPU one without touching the request path.
//! the trait deliberately speaks in pixmaps, paths, paints; it does not name
//! `tiny-skia` types. concrete impls live in `mars-render`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::sync::Arc;

use mars_style::{ResolvedLabelStyle, ResolvedStyle};

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
    /// A style referenced an image resource by name, but the renderer's
    /// [`ImageRegistry`] has no entry for that name. Distinct from
    /// `NotImplemented` because the surface exists; the asset is missing.
    #[error("image resource not found: {name}")]
    ImageNotFound {
        /// Resource name as stored in the style.
        name: String,
    },
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

/// Axis-aligned destination rectangle in render-target pixel space.
/// Floating point so sub-pixel placement survives without an extra
/// rounding step at the seam.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PixelRect {
    /// Top-left x in pixel space.
    pub x: f32,
    /// Top-left y in pixel space.
    pub y: f32,
    /// Width in pixels.
    pub w: f32,
    /// Height in pixels.
    pub h: f32,
}

/// A decoded raster image: RGBA8, row-major, `width * height * 4` bytes.
/// Used both for tiled image fills ([`DrawOp::Pattern`] image variant) and
/// for raster-tile compositing ([`DrawOp::Raster`]).
#[derive(Debug, Clone)]
pub struct DecodedImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Straight (non-premultiplied) RGBA bytes, row-major.
    pub rgba: Arc<Vec<u8>>,
}

/// Renderer-side registry resolving bitmap names to decoded RGBA. Mirrors
/// the `Fonts` registry's role: the runtime builds one at manifest load
/// from a bundled image artifact and hands it to the renderer.
///
/// `get` is hot-path; concrete impls back it by an `Arc<Vec<u8>>` clone so
/// the renderer holds a cheap reference for the duration of one render.
pub trait ImageRegistry: std::fmt::Debug + Send + Sync + 'static {
    /// Look up an image by its registered name. `None` means the manifest
    /// did not bundle this name; callers surface
    /// [`RenderError::ImageNotFound`] from the relevant dispatch.
    fn get(&self, name: &str) -> Option<Arc<DecodedImage>>;
}

/// A registry that knows about no images. Used as the safe default in
/// renderer construction sites that do not (yet) thread a bundled image
/// artifact - styles referencing an image then surface
/// [`RenderError::ImageNotFound`] from the dispatch hub.
#[derive(Debug, Default, Clone, Copy)]
pub struct EmptyImageRegistry;

impl ImageRegistry for EmptyImageRegistry {
    fn get(&self, _name: &str) -> Option<Arc<DecodedImage>> {
        None
    }
}

/// One draw operation. Intentionally narrow - adding shapes goes through this
/// enum so the type system carries the contract: the runtime emits a variant,
/// the renderer is statically required to handle it.
#[derive(Debug, Clone)]
pub enum DrawOp {
    /// Fill or stroke a path with the given style.
    Path {
        /// Geometry to draw.
        path: Path,
        /// Fill / stroke style. Already resolved against the request denom;
        /// the renderer never sees `ScaledSize`.
        style: Arc<ResolvedStyle>,
    },
    /// Place a label glyph run at an anchor point with the given style.
    Label {
        /// Anchor in pixel space (baseline).
        anchor: (f32, f32),
        /// Plain-text content; renderer shapes and rasterises.
        text: String,
        /// Compiled label style. Already resolved against the request denom.
        style: Arc<ResolvedLabelStyle>,
        /// Counter-clockwise rotation in radians. `0.0` for axis-aligned
        /// labels (the common case); line labels carry a tangent angle.
        angle_rad: f32,
    },
    /// Place a label glyph run along a polyline, rotating each glyph to
    /// its own local tangent (ANGLE FOLLOW). distinct from [`DrawOp::Label`]
    /// because the run is not a single affine transform; the renderer
    /// shapes the run, walks per-glyph advances along the polyline, and
    /// stamps each glyph individually.
    FollowLabel {
        /// Pixel-space polyline. already projected and CRS-transformed by
        /// the runtime; the renderer just walks arc-lengths along it.
        polyline_px: Vec<(f32, f32)>,
        /// Arc-length along `polyline_px` (in pixels) at which the first
        /// glyph's left edge sits. the run is centred by the runtime
        /// setting this to `centre_arc - total_advance / 2`.
        start_arc_px: f32,
        /// Plain-text content; renderer shapes and rasterises per-glyph.
        text: String,
        /// Compiled label style. Already resolved against the request denom.
        style: Arc<ResolvedLabelStyle>,
    },
    /// Place a point-anchored marker symbol. Use this from the runtime when a
    /// symbol cannot be tessellated to a [`DrawOp::Path`] (text glyphs,
    /// future svg / raster markers); for shapes the runtime still
    /// tessellates (circle, square, vector polygon), keep emitting `Path`.
    Symbol {
        /// Anchor in pixel space.
        anchor: (f32, f32),
        /// Counter-clockwise rotation in radians.
        rotation_rad: f32,
        /// Style. The `marker` field carries the symbol kind; fill / stroke
        /// fields apply to the rasterised symbol.
        style: Arc<ResolvedStyle>,
    },
    /// Fill a path with a non-procedural pattern (image, svg, future
    /// gradient). Procedural fills (solid, hatch) continue to flow through
    /// [`DrawOp::Path`].
    Pattern {
        /// Geometry to fill.
        path: Path,
        /// Style. The `fill` paint variant carries the pattern descriptor.
        style: Arc<ResolvedStyle>,
    },
    /// Composite a decoded raster tile onto a destination rectangle. Used
    /// by raster layers - the runtime fetches and decodes the tile, the
    /// renderer paints it at the requested rect with the requested opacity.
    Raster {
        /// Decoded RGBA tile.
        tile: Arc<DecodedImage>,
        /// Destination rectangle in pixel space.
        dst: PixelRect,
        /// Per-op opacity multiplier in `[0.0, 1.0]`. Composed with the
        /// tile's own alpha.
        opacity: f32,
        /// Compositing operator. `None` falls back to the rasteriser's
        /// source-over default. Threaded through so a raster source can
        /// emit a non-default blend mode (multiply, screen, ...) just like
        /// a vector pass.
        blend_mode: Option<mars_style::BlendMode>,
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
    fn measure_text(&self, text: &str, style: &ResolvedLabelStyle) -> Result<TextMetrics, RenderError>;
}

/// Encoder port. Splits image-format encoding from rasterisation so the two
/// concerns can evolve independently (e.g. swap PNG library, add JPEG).
pub trait Encoder: Send + Sync + 'static {
    /// Encode `pixmap` to `format`-specific bytes (PNG / JPEG / ...).
    fn encode(&self, pixmap: &Pixmap, format: ImageFormat) -> Result<Vec<u8>, EncodeError>;
}

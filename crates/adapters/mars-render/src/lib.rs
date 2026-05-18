//! CPU rasteriser. tiny-skia rasterisation, PNG, JPEG and WebP encoding.

#![forbid(unsafe_code)]

mod canvas;
mod decoded_image;
mod encode;
mod fill;
mod label;
mod ops;
mod path;
mod path_offset;
mod pattern;
mod polyline;
mod prepare;
mod raster;
mod stroke;
mod surface;
mod symbol;

use std::sync::Arc;

use mars_render_port::{
    Canvas, DrawOp, EmptyImageRegistry, EncodeError, Encoder, ImageFormat, ImageRegistry, Pixmap, RenderError,
    Renderer, Surface, TextMetrics, dispatch_ops,
};
use mars_style::ResolvedLabelStyle;
use mars_text::{FontError, Fonts};

use crate::surface::TinySkiaSurface;

/// CPU rasteriser. Stamps geometry via tiny-skia and shapes / rasterises
/// label runs via [`mars_text`]. The font registry is shared with the
/// runtime collision pass so both agree on glyph metrics. The image
/// registry resolves `FillPaint::Image { name }` and `DrawOp::Raster`
/// references when the manifest bundled an image pack.
#[derive(Debug)]
pub struct TinySkiaRenderer {
    fonts: Arc<Fonts>,
    images: Arc<dyn ImageRegistry>,
}

impl TinySkiaRenderer {
    /// Construct with the supplied font registry. Image resolution falls
    /// through to [`EmptyImageRegistry`]; styles referencing an image will
    /// surface [`RenderError::ImageNotFound`].
    #[must_use]
    pub fn new(fonts: Arc<Fonts>) -> Self {
        Self {
            fonts,
            images: Arc::new(EmptyImageRegistry),
        }
    }

    /// Construct with both registries. Production wiring uses this; tests
    /// and call sites with no image pack can keep using [`Self::new`].
    #[must_use]
    pub fn with_images(fonts: Arc<Fonts>, images: Arc<dyn ImageRegistry>) -> Self {
        Self { fonts, images }
    }
}

impl Renderer for TinySkiaRenderer {
    fn render(&self, canvas: Canvas, ops: &[DrawOp]) -> Result<Pixmap, RenderError> {
        if canvas.width == 0 || canvas.height == 0 {
            return Err(RenderError::Backend("canvas has zero dimension".into()));
        }

        let mut surface: Box<dyn Surface> = Box::new(TinySkiaSurface::new(
            canvas.width,
            canvas.height,
            self.fonts.clone(),
            self.images.clone(),
        )?);

        if let Some(bg) = canvas.background {
            surface.fill_background(bg);
        }

        dispatch_ops(surface.as_mut(), ops)?;

        Ok(surface.finish())
    }

    fn measure_text(&self, text: &str, style: &ResolvedLabelStyle) -> Result<TextMetrics, RenderError> {
        let run = mars_text::measure(text, style, &self.fonts).map_err(font_err_to_render)?;
        Ok(TextMetrics {
            advance_x: run.advance_x,
            ascent: run.ascent,
            descent: run.descent,
        })
    }
}

fn font_err_to_render(e: FontError) -> RenderError {
    RenderError::Backend(format!("font: {e}"))
}

/// PNG deflate level used by [`TinySkiaEncoder`]. Variants intentionally
/// mirror `png::Compression` so a config layer (e.g. `mars-config`) can carry
/// the same enum shape without depending on the `png` crate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PngCompression {
    /// No compression. Largest files, fastest encode.
    None,
    /// Lightest compression (≈ deflate level 1 via fdeflate's fast path).
    Fastest,
    /// Solid speed/ratio tradeoff suited to ephemeral tile responses.
    #[default]
    Fast,
    /// Default of the `png` crate (≈ deflate level 6).
    Balanced,
    /// Smallest output, slowest encode.
    High,
}

impl PngCompression {
    pub(crate) fn to_png(self) -> png::Compression {
        match self {
            Self::None => png::Compression::NoCompression,
            Self::Fastest => png::Compression::Fastest,
            Self::Fast => png::Compression::Fast,
            Self::Balanced => png::Compression::Balanced,
            Self::High => png::Compression::High,
        }
    }
}

/// PNG, JPEG and WebP encoder. Lives next to the rasteriser so the in-process
/// pipeline does not pay an extra copy crossing crate boundaries.
///
/// JPEG quality and PNG deflate level are captured at construction so they
/// don't have to leak into the [`Encoder::encode`] trait signature. WebP is
/// lossless and carries no tuning knob.
#[derive(Debug, Clone, Copy)]
pub struct TinySkiaEncoder {
    jpeg_quality: u8,
    png_compression: PngCompression,
}

impl TinySkiaEncoder {
    /// Construct with configured jpeg quality (1-100) and PNG deflate level.
    #[must_use]
    pub fn new(jpeg_quality: u8, png_compression: PngCompression) -> Self {
        Self {
            jpeg_quality,
            png_compression,
        }
    }
}

impl Default for TinySkiaEncoder {
    fn default() -> Self {
        Self {
            jpeg_quality: 85,
            png_compression: PngCompression::default(),
        }
    }
}

impl Encoder for TinySkiaEncoder {
    fn encode(&self, pixmap: &Pixmap, format: ImageFormat) -> Result<Vec<u8>, EncodeError> {
        match format {
            ImageFormat::Png => encode::encode_png(pixmap, self.png_compression),
            ImageFormat::Jpeg => encode::encode_jpeg(pixmap, self.jpeg_quality),
            ImageFormat::Webp => encode::encode_webp(pixmap),
        }
    }
}

#[cfg(test)]
mod tests;

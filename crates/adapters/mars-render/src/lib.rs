//! CPU rasteriser. tiny-skia rasterisation, PNG and JPEG encoding.

#![forbid(unsafe_code)]

mod canvas;
mod encode;
mod fill;
mod label;
mod ops;
mod path;
mod path_offset;
mod prepare;
mod stroke;

use std::sync::Arc;

use mars_render_port::{Canvas, DrawOp, EncodeError, Encoder, ImageFormat, Pixmap, RenderError, Renderer, TextMetrics};
use mars_style::LabelStyle;
use mars_text::{FontError, Fonts};
use tiny_skia::Pixmap as SkPixmap;

/// CPU rasteriser. Stamps geometry via tiny-skia and shapes / rasterises
/// label runs via [`mars_text`]. The font registry is shared with the
/// runtime collision pass so both agree on glyph metrics.
#[derive(Debug)]
pub struct TinySkiaRenderer {
    fonts: Arc<Fonts>,
}

impl TinySkiaRenderer {
    /// Construct with the supplied font registry.
    #[must_use]
    pub fn new(fonts: Arc<Fonts>) -> Self {
        Self { fonts }
    }
}

impl Renderer for TinySkiaRenderer {
    fn render(&self, canvas: Canvas, ops: &[DrawOp]) -> Result<Pixmap, RenderError> {
        if canvas.width == 0 || canvas.height == 0 {
            return Err(RenderError::Backend("canvas has zero dimension".into()));
        }

        let mut pm = SkPixmap::new(canvas.width, canvas.height)
            .ok_or_else(|| RenderError::Backend(format!("pixmap alloc {}x{}", canvas.width, canvas.height)))?;

        if let Some(bg) = canvas.background {
            crate::canvas::fill_background(&mut pm, bg);
        }

        for op in ops {
            ops::dispatch(&mut pm, op, &self.fonts)?;
        }

        let width = pm.width();
        let height = pm.height();

        Ok(Pixmap {
            width,
            height,
            premultiplied_rgba: pm.take(),
        })
    }

    fn measure_text(&self, text: &str, style: &LabelStyle) -> Result<TextMetrics, RenderError> {
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

/// PNG and JPEG encoder. Lives next to the rasteriser so the in-process
/// pipeline does not pay an extra copy crossing crate boundaries.
///
/// JPEG quality and PNG deflate level are captured at construction so they
/// don't have to leak into the [`Encoder::encode`] trait signature.
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
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use mars_render_port::{Path as PortPath, Subpath};
    use mars_style::{Colour, FillPaint, Style};

    fn red() -> Colour {
        Colour {
            r: 255,
            g: 0,
            b: 0,
            a: 255,
        }
    }
    fn white() -> Colour {
        Colour {
            r: 255,
            g: 255,
            b: 255,
            a: 255,
        }
    }

    fn square(cx: f32, cy: f32, half: f32) -> PortPath {
        PortPath {
            subpaths: vec![Subpath {
                points: vec![
                    (cx - half, cy - half),
                    (cx + half, cy - half),
                    (cx + half, cy + half),
                    (cx - half, cy + half),
                ],
                closed: true,
            }],
        }
    }

    fn render_png(canvas: Canvas, ops: &[DrawOp]) -> Vec<u8> {
        let pm = TinySkiaRenderer::new(std::sync::Arc::new(mars_text::Fonts::with_default()))
            .render(canvas, ops)
            .unwrap();
        TinySkiaEncoder::default().encode(&pm, ImageFormat::Png).unwrap()
    }

    fn decode(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let dec = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = dec.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        buf.truncate(info.buffer_size());
        (info.width, info.height, buf)
    }

    #[test]
    fn determinism_byte_exact() {
        let canvas = Canvas {
            width: 64,
            height: 64,
            background: Some(white()),
        };
        let ops = vec![DrawOp::Path {
            path: square(32.0, 32.0, 16.0),
            style: Arc::new(Style {
                fill: Some(FillPaint::Solid(red())),
                ..Default::default()
            }),
        }];
        let a = render_png(canvas, &ops);
        let b = render_png(canvas, &ops);
        assert_eq!(a, b, "renderer must be deterministic");
    }

    /// drives the same `jpeg_encoder` / `jpeg_decoder` pair the diff harness
    /// uses (`bin/mars-diff-capture/src/coverage.rs::coverage_jpeg`). exercised
    /// at three sizes so an mcu-boundary regression in the encoder cannot
    /// hide behind the 16x16 single-mcu degenerate case: 16 = 1 mcu per side,
    /// 512 = exactly 32 mcus per side (matches the `composite-urban-detail-jpeg`
    /// case dimensions), 510 = non-multiple-of-16 chroma-upsampling boundary.
    #[test]
    fn jpeg_roundtrip_decodes_to_expected_dimensions() {
        for &dim in &[16u32, 510, 512] {
            let canvas = Canvas {
                width: dim,
                height: dim,
                background: Some(red()),
            };
            let pm = TinySkiaRenderer::new(std::sync::Arc::new(mars_text::Fonts::with_default()))
                .render(canvas, &[])
                .unwrap();
            let bytes = TinySkiaEncoder::default().encode(&pm, ImageFormat::Jpeg).unwrap();
            assert!(bytes.starts_with(&[0xFF, 0xD8]), "jpeg SOI marker at {dim}x{dim}");

            let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(&bytes));
            let pixels = dec.decode().unwrap();
            let info = dec.info().unwrap();
            assert_eq!((info.width, info.height), (dim as u16, dim as u16), "dim {dim}");
            // harness coverage_jpeg only accepts RGB24 / L8; if jpeg_encoder
            // ever changes default colour space the harness silently regresses
            // to coverage=None. assert here so the test fails first.
            assert_eq!(
                info.pixel_format,
                jpeg_decoder::PixelFormat::RGB24,
                "harness coverage path expects RGB24 at {dim}x{dim}"
            );

            // mid-image sample (not corner) so chroma-upsampling artefacts at
            // the right/bottom edge of non-multiple-of-16 sizes can't hide a
            // colour-channel swap.
            let mid = ((dim as usize / 2) * dim as usize + (dim as usize / 2)) * 3;
            let (r, g, b) = (pixels[mid], pixels[mid + 1], pixels[mid + 2]);
            assert!(
                r > 200 && g < 60 && b < 60,
                "expected red-ish at {dim}x{dim}, got ({r},{g},{b})"
            );
        }
    }

    #[test]
    fn png_compression_levels_decode_to_identical_pixels() {
        let canvas = Canvas {
            width: 32,
            height: 32,
            background: Some(white()),
        };
        let ops = vec![DrawOp::Path {
            path: square(16.0, 16.0, 8.0),
            style: Arc::new(Style {
                fill: Some(FillPaint::Solid(red())),
                ..Default::default()
            }),
        }];
        let pm = TinySkiaRenderer::new(std::sync::Arc::new(mars_text::Fonts::with_default()))
            .render(canvas, &ops)
            .unwrap();
        let levels = [
            PngCompression::None,
            PngCompression::Fastest,
            PngCompression::Fast,
            PngCompression::Balanced,
            PngCompression::High,
        ];
        let decoded: Vec<_> = levels
            .iter()
            .map(|&c| {
                let enc = TinySkiaEncoder::new(85, c);
                let bytes = enc.encode(&pm, ImageFormat::Png).unwrap();
                decode(&bytes)
            })
            .collect();
        // every level must round-trip to the same pixel buffer; encoded bytes
        // legitimately differ.
        let (w0, h0, ref rgba0) = decoded[0];
        for (i, (w, h, rgba)) in decoded.iter().enumerate().skip(1) {
            assert_eq!((*w, *h), (w0, h0), "level {i} dimension mismatch");
            assert_eq!(rgba, rgba0, "level {i} pixel mismatch");
        }
    }

    #[test]
    fn jpeg_transparent_collapses_to_white() {
        let canvas = Canvas {
            width: 8,
            height: 8,
            background: None,
        };
        let pm = TinySkiaRenderer::new(std::sync::Arc::new(mars_text::Fonts::with_default()))
            .render(canvas, &[])
            .unwrap();
        let bytes = TinySkiaEncoder::default().encode(&pm, ImageFormat::Jpeg).unwrap();
        let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(&bytes));
        let pixels = dec.decode().unwrap();
        let (r, g, b) = (pixels[0], pixels[1], pixels[2]);
        assert!(r > 240 && g > 240 && b > 240, "expected white-ish, got ({r},{g},{b})");
    }

    #[test]
    fn golden_square_matches() {
        let canvas = Canvas {
            width: 64,
            height: 64,
            background: Some(white()),
        };
        let ops = vec![DrawOp::Path {
            path: square(32.0, 32.0, 16.0),
            style: Arc::new(Style {
                fill: Some(FillPaint::Solid(red())),
                ..Default::default()
            }),
        }];
        let actual = render_png(canvas, &ops);
        let (w1, h1, rgba1) = decode(&actual);

        let golden_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/square.png");
        if std::env::var("MARS_UPDATE_GOLDEN").is_ok() || !golden_path.exists() {
            std::fs::create_dir_all(golden_path.parent().unwrap()).unwrap();
            std::fs::write(&golden_path, &actual).unwrap();
        }
        let expected_bytes = std::fs::read(&golden_path).unwrap();
        let (w2, h2, rgba2) = decode(&expected_bytes);
        assert_eq!((w1, h1), (w2, h2), "golden dimension mismatch");

        // pixel-level comparison tolerates platform differences in png compression.
        let mismatches = rgba1
            .chunks_exact(4)
            .zip(rgba2.chunks_exact(4))
            .filter(|(a, b)| a.iter().zip(b.iter()).any(|(x, y)| x.abs_diff(*y) > 1))
            .count();
        assert_eq!(
            mismatches, 0,
            "golden pixel mismatch ({mismatches} pixels); rerun with MARS_UPDATE_GOLDEN=1 if intentional"
        );
    }
}

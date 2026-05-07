//! CPU rasteriser. SPEC §11.2 - tiny-skia rasterisation, PNG and JPEG encoding.

#![forbid(unsafe_code)]

mod encode;
mod raster;

use std::sync::Arc;

use mars_render_port::{Canvas, DrawOp, EncodeError, Encoder, ImageFormat, Pixmap, RenderError, Renderer};
use mars_text::Fonts;
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
            raster::fill_background(&mut pm, bg);
        }

        for op in ops {
            match op {
                DrawOp::Path { path, style } => raster::draw_path(&mut pm, path, style),
                DrawOp::Label { anchor, text, style } => {
                    raster::draw_label(&mut pm, *anchor, text, style, &self.fonts)?;
                }
            }
        }

        Ok(Pixmap {
            width: pm.width(),
            height: pm.height(),
            premultiplied_rgba: pm.data().to_vec(),
        })
    }
}

/// PNG and JPEG encoder. Lives next to the rasteriser so the in-process
/// pipeline does not pay an extra copy crossing crate boundaries.
///
/// JPEG quality is captured at construction so it does not have to leak into
/// the [`Encoder::encode`] trait signature.
#[derive(Debug, Clone, Copy)]
pub struct TinySkiaEncoder {
    jpeg_quality: u8,
}

impl TinySkiaEncoder {
    /// Construct with a configured jpeg quality (1-100).
    #[must_use]
    pub fn new(jpeg_quality: u8) -> Self {
        Self { jpeg_quality }
    }
}

impl Default for TinySkiaEncoder {
    fn default() -> Self {
        Self { jpeg_quality: 85 }
    }
}

impl Encoder for TinySkiaEncoder {
    fn encode(&self, pixmap: &Pixmap, format: ImageFormat) -> Result<Vec<u8>, EncodeError> {
        match format {
            ImageFormat::Png => encode::encode_png(pixmap),
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
    use mars_style::{Colour, Style};

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
                fill: Some(red()),
                ..Default::default()
            }),
        }];
        let a = render_png(canvas, &ops);
        let b = render_png(canvas, &ops);
        assert_eq!(a, b, "renderer must be deterministic");
    }

    #[test]
    fn filled_polygon_red_pixels() {
        let canvas = Canvas {
            width: 64,
            height: 64,
            background: None,
        };
        let ops = vec![DrawOp::Path {
            path: square(32.0, 32.0, 16.0),
            style: Arc::new(Style {
                fill: Some(red()),
                ..Default::default()
            }),
        }];
        let png_bytes = render_png(canvas, &ops);
        let (_, _, rgba) = decode(&png_bytes);
        let red_count = rgba
            .chunks_exact(4)
            .filter(|p| p[0] > 200 && p[1] < 40 && p[2] < 40 && p[3] == 255)
            .count();
        assert!(red_count > 800, "expected >800 red pixels, got {red_count}");
    }

    #[test]
    fn stroked_line_has_pixels() {
        let canvas = Canvas {
            width: 64,
            height: 64,
            background: None,
        };
        let path = PortPath {
            subpaths: vec![Subpath {
                points: vec![(8.0, 32.0), (56.0, 32.0)],
                closed: false,
            }],
        };
        let ops = vec![DrawOp::Path {
            path,
            style: Arc::new(Style {
                stroke: Some(red()),
                stroke_width: Some(2.0),
                ..Default::default()
            }),
        }];
        let png_bytes = render_png(canvas, &ops);
        let (w, _, rgba) = decode(&png_bytes);
        let row = 32usize * w as usize * 4;
        let on_row: usize = rgba[row..row + w as usize * 4]
            .chunks_exact(4)
            .filter(|p| p[3] > 0)
            .count();
        assert!(on_row >= 40, "expected stroked pixels on row 32, got {on_row}");
    }

    #[test]
    fn open_linestring_does_not_close() {
        // a v-shaped open line drawn near the top-left corner should not have
        // pixels in the bottom-right, which it would if tiny-skia closed it.
        let canvas = Canvas {
            width: 64,
            height: 64,
            background: None,
        };
        let path = PortPath {
            subpaths: vec![Subpath {
                points: vec![(10.0, 10.0), (30.0, 10.0), (30.0, 30.0)],
                closed: false,
            }],
        };
        let ops = vec![DrawOp::Path {
            path,
            style: Arc::new(Style {
                stroke: Some(red()),
                stroke_width: Some(1.0),
                ..Default::default()
            }),
        }];
        let png_bytes = render_png(canvas, &ops);
        let (w, _, rgba) = decode(&png_bytes);
        // bottom-right corner should remain transparent
        let br = ((w - 2) * 4) as usize + ((w * (canvas.height - 2)) * 4) as usize;
        assert_eq!(rgba[br + 3], 0, "open linestring must not close to bottom-right");
    }

    #[test]
    fn transparent_vs_opaque_background() {
        let c1 = Canvas {
            width: 4,
            height: 4,
            background: None,
        };
        let png1 = render_png(c1, &[]);
        let (_, _, rgba1) = decode(&png1);
        assert_eq!(rgba1[3], 0, "transparent bg first pixel alpha 0");

        let c2 = Canvas {
            width: 4,
            height: 4,
            background: Some(white()),
        };
        let png2 = render_png(c2, &[]);
        let (_, _, rgba2) = decode(&png2);
        assert_eq!(rgba2[3], 255, "opaque bg first pixel alpha 255");
        assert_eq!(&rgba2[0..3], &[255, 255, 255]);
    }

    #[test]
    fn label_op_is_skipped_not_errored() {
        let canvas = Canvas {
            width: 8,
            height: 8,
            background: None,
        };
        let ops = vec![DrawOp::Label {
            anchor: (0.0, 0.0),
            text: "hi".into(),
            style: Arc::new(mars_style::LabelStyle {
                font_family: "DejaVu Sans".into(),
                font_size: 12.0,
                fill: mars_style::Colour::rgba(0, 0, 0, 255),
                halo: None,
                priority: 0,
                min_distance: 0.0,
            }),
        }];
        let pm = TinySkiaRenderer::new(std::sync::Arc::new(mars_text::Fonts::with_default())).render(canvas, &ops);
        assert!(pm.is_ok(), "label op should be skipped, not error: {pm:?}");
    }

    #[test]
    fn jpeg_roundtrip_decodes_to_expected_dimensions() {
        let canvas = Canvas {
            width: 16,
            height: 16,
            background: Some(red()),
        };
        let pm = TinySkiaRenderer::new(std::sync::Arc::new(mars_text::Fonts::with_default()))
            .render(canvas, &[])
            .unwrap();
        let bytes = TinySkiaEncoder::default().encode(&pm, ImageFormat::Jpeg).unwrap();
        assert!(bytes.starts_with(&[0xFF, 0xD8]), "jpeg SOI marker");

        let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(&bytes));
        let pixels = dec.decode().unwrap();
        let info = dec.info().unwrap();
        assert_eq!((info.width, info.height), (16, 16));
        assert_eq!(info.pixel_format, jpeg_decoder::PixelFormat::RGB24);

        // sanity-check colour is roughly red after lossy round-trip.
        let (r, g, b) = (pixels[0], pixels[1], pixels[2]);
        assert!(r > 200 && g < 60 && b < 60, "expected red-ish, got ({r},{g},{b})");
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
                fill: Some(red()),
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

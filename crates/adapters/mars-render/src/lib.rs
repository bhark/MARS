//! CPU rasteriser. tiny-skia rasterisation, PNG and JPEG encoding.

#![forbid(unsafe_code)]

mod encode;
mod raster;

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
            raster::fill_background(&mut pm, bg);
        }

        for op in ops {
            match op {
                DrawOp::Path { path, style } => raster::draw_path(&mut pm, path, style),
                DrawOp::Label {
                    anchor,
                    text,
                    style,
                    angle_rad,
                } => {
                    raster::draw_label(&mut pm, *anchor, text, style, *angle_rad, &self.fonts)?;
                }
            }
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
    fn measure_text_returns_font_aware_metrics() {
        let r = TinySkiaRenderer::new(Arc::new(mars_text::Fonts::with_default()));
        let style = mars_style::LabelStyle {
            font_family: "DejaVu Sans".into(),
            font_size: 12.0,
            fill: mars_style::Colour::rgba(0, 0, 0, 255),
            halo: None,
            priority: 0,
            min_distance: 0.0,
        };
        let m = r.measure_text("hello", &style).unwrap();
        // shaped advance of "hello" at 12px is well over the chars*0.55*fs
        // approximation's lower bound but still far below a worst-case glyph
        // run; just sanity-check the metric is finite and positive.
        assert!(m.advance_x.is_finite() && m.advance_x > 0.0);
        assert!(m.ascent.is_finite() && m.ascent > 0.0);
        assert!(m.descent.is_finite() && m.descent >= 0.0);
        // empty text shapes to a zero-width run.
        let zero = r.measure_text("", &style).unwrap();
        assert_eq!(zero.advance_x, 0.0);
        // ascent + descent depend only on the font, not the text.
        assert!((m.ascent - zero.ascent).abs() < 1e-3);
    }

    #[test]
    fn measure_text_unknown_font_falls_back_to_default() {
        // mars_text::Fonts::select falls back to DejaVu Sans / sans-serif on
        // unknown family rather than erroring; the measure_text path inherits
        // that contract so labels render with a sane default.
        let r = TinySkiaRenderer::new(Arc::new(mars_text::Fonts::with_default()));
        let style = mars_style::LabelStyle {
            font_family: "no-such-font-12345".into(),
            font_size: 12.0,
            fill: mars_style::Colour::rgba(0, 0, 0, 255),
            halo: None,
            priority: 0,
            min_distance: 0.0,
        };
        let m = r.measure_text("x", &style).unwrap();
        assert!(m.advance_x > 0.0 && m.ascent > 0.0);
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
                fill: Some(FillPaint::Solid(red())),
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
    fn collapsed_polygon_fill_is_silently_skipped() {
        // a closed ring whose vertices all share the same y (typical of a tiny
        // polygon collapsed onto a pixel row by world->pixel projection at
        // coarse zoom). tiny-skia's fill_path would log::warn + no-op; we
        // gate ahead of it so the call never happens. behavioural check: the
        // canvas remains empty (no fill drawn) and the renderer doesn't error.
        let canvas = Canvas {
            width: 32,
            height: 32,
            background: None,
        };
        let path = PortPath {
            subpaths: vec![Subpath {
                points: vec![(4.0, 16.0), (16.0, 16.0), (28.0, 16.0)],
                closed: true,
            }],
        };
        let ops = vec![DrawOp::Path {
            path,
            style: Arc::new(Style {
                fill: Some(FillPaint::Solid(red())),
                ..Default::default()
            }),
        }];
        let png_bytes = render_png(canvas, &ops);
        let (_, _, rgba) = decode(&png_bytes);
        let opaque_count = rgba.chunks_exact(4).filter(|p| p[3] != 0).count();
        assert_eq!(opaque_count, 0, "degenerate-bbox fill must paint nothing");
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
            angle_rad: 0.0,
        }];
        let pm = TinySkiaRenderer::new(std::sync::Arc::new(mars_text::Fonts::with_default())).render(canvas, &ops);
        assert!(pm.is_ok(), "label op should be skipped, not error: {pm:?}");
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
    fn subpixel_stroke_0_15_is_visible_and_soft() {
        // a 0.15 px stroke must be visible (regression: tiny-skia drops thin
        // strokes outright) AND must be soft, not full-intensity. AGG-style
        // emulation = 1px stroke at proportional alpha. concretely: edge
        // pixels are tinted toward stroke from fill, but never approach the
        // raw stroke colour.
        let canvas = Canvas {
            width: 64,
            height: 64,
            background: Some(white()),
        };
        let ops = vec![DrawOp::Path {
            path: square(32.0, 32.0, 16.0),
            style: Arc::new(Style {
                fill: Some(FillPaint::Solid(mars_style::Colour::rgb(220, 240, 255))),
                stroke: Some(mars_style::Colour::rgb(40, 150, 230)),
                stroke_width: Some(0.15),
                ..Default::default()
            }),
        }];
        let (_, _, rgba) = decode(&render_png(canvas, &ops));

        // pixels with red strictly below pure fill (220) require stroke
        // contribution: pure white->fill anti-alias only produces r in
        // [220, 255]. their existence proves the stroke actually rendered.
        let stroke_tinted = rgba.chunks_exact(4).filter(|p| p[0] < 220 && p[3] > 0).count();
        assert!(stroke_tinted > 0, "sub-pixel stroke produced no visible tint");

        // and: no pixel should be saturated toward the raw stroke colour.
        // a full-alpha 1px stroke would produce pixels with r near 40; under
        // 15% alpha-scaled emulation r should stay well above ~150.
        let saturated = rgba.chunks_exact(4).filter(|p| p[0] < 120).count();
        assert_eq!(
            saturated, 0,
            "sub-pixel stroke is rendering at full intensity ({saturated} saturated px)"
        );
    }

    #[test]
    fn hatch_fill_paints_lines_inside_polygon() {
        // a centred 24x24 square on a 64x64 black canvas, hatch-filled with
        // red lines at 4px spacing and 0deg (horizontal). expect:
        //   - red-ish pixels inside the square's bbox.
        //   - no red-ish pixels outside the square's bbox (clip mask works).
        //   - roughly the right number of hatch lines (24 / 4 = ~6).
        let canvas = Canvas {
            width: 64,
            height: 64,
            background: None,
        };
        let ops = vec![DrawOp::Path {
            path: square(32.0, 32.0, 12.0),
            style: Arc::new(Style {
                fill: Some(FillPaint::Hatch {
                    spacing: 4.0,
                    angle_deg: 0.0,
                    line_width: 1.0,
                    colour: Colour::rgba(220, 30, 30, 255),
                }),
                ..Default::default()
            }),
        }];
        let (w, _, rgba) = decode(&render_png(canvas, &ops));
        // strong-red pixels (anti-aliasing means we accept r > 150, g < 80).
        let is_red = |p: &[u8]| p[0] > 150 && p[1] < 80 && p[2] < 80 && p[3] > 0;

        // pixel ranges based on square(cx=32, cy=32, half=12) -> x in [20, 44).
        let inside_red = rgba
            .chunks_exact(4)
            .enumerate()
            .filter(|(i, p)| {
                let x = (i % w as usize) as f32;
                let y = (i / w as usize) as f32;
                is_red(p) && (20.0..44.0).contains(&x) && (20.0..44.0).contains(&y)
            })
            .count();
        assert!(inside_red > 100, "hatch produced too few inside-poly red pixels: {inside_red}");

        let outside_red = rgba
            .chunks_exact(4)
            .enumerate()
            .filter(|(i, p)| {
                let x = (i % w as usize) as f32;
                let y = (i / w as usize) as f32;
                is_red(p) && !((20.0..44.0).contains(&x) && (20.0..44.0).contains(&y))
            })
            .count();
        assert_eq!(outside_red, 0, "hatch leaked outside polygon clip mask: {outside_red} px");
    }

    #[test]
    fn hatch_fill_at_45_degrees_paints_inside_polygon() {
        // diagonal hatch should still produce red pixels inside the polygon.
        let canvas = Canvas {
            width: 64,
            height: 64,
            background: None,
        };
        let ops = vec![DrawOp::Path {
            path: square(32.0, 32.0, 12.0),
            style: Arc::new(Style {
                fill: Some(FillPaint::Hatch {
                    spacing: 4.0,
                    angle_deg: 45.0,
                    line_width: 1.0,
                    colour: Colour::rgba(220, 30, 30, 255),
                }),
                ..Default::default()
            }),
        }];
        let (_, _, rgba) = decode(&render_png(canvas, &ops));
        let red_count = rgba
            .chunks_exact(4)
            .filter(|p| p[0] > 150 && p[1] < 80 && p[2] < 80 && p[3] > 0)
            .count();
        assert!(red_count > 100, "diagonal hatch produced too few red pixels: {red_count}");
    }

    #[test]
    fn hatch_fill_outline_still_strokes() {
        // hatch fill must not suppress the stroke arm; a polygon with both
        // hatch fill and a stroke must show stroke pixels along its boundary.
        let canvas = Canvas {
            width: 64,
            height: 64,
            background: None,
        };
        let ops = vec![DrawOp::Path {
            path: square(32.0, 32.0, 12.0),
            style: Arc::new(Style {
                fill: Some(FillPaint::Hatch {
                    spacing: 6.0,
                    angle_deg: 30.0,
                    line_width: 1.0,
                    colour: Colour::rgba(220, 30, 30, 255),
                }),
                stroke: Some(Colour::rgba(0, 200, 0, 255)),
                stroke_width: Some(2.0),
                ..Default::default()
            }),
        }];
        let (_, _, rgba) = decode(&render_png(canvas, &ops));
        let green_count = rgba
            .chunks_exact(4)
            .filter(|p| p[1] > 150 && p[0] < 80 && p[2] < 80 && p[3] > 0)
            .count();
        assert!(green_count > 50, "outline stroke missing: only {green_count} green px");
    }

    #[test]
    fn rotated_label_lands_in_rotated_bbox() {
        // 90-degree rotation: a horizontal label anchored at the centre
        // should produce painted pixels along a vertical strip, not a
        // horizontal one. proves the rotation actually rotates the mask.
        let canvas = Canvas {
            width: 64,
            height: 64,
            background: None,
        };
        let label_style = Arc::new(mars_style::LabelStyle {
            font_family: "DejaVu Sans".into(),
            font_size: 16.0,
            fill: mars_style::Colour::rgba(0, 0, 0, 255),
            halo: None,
            priority: 0,
            min_distance: 0.0,
        });
        let upright = vec![DrawOp::Label {
            anchor: (32.0, 32.0),
            text: "ABC".into(),
            style: label_style.clone(),
            angle_rad: 0.0,
        }];
        let rotated = vec![DrawOp::Label {
            anchor: (32.0, 32.0),
            text: "ABC".into(),
            style: label_style,
            angle_rad: std::f32::consts::FRAC_PI_2,
        }];
        let (w, _, up_rgba) = decode(&render_png(canvas, &upright));
        let (_, _, rot_rgba) = decode(&render_png(canvas, &rotated));

        let painted_extents = |rgba: &[u8]| {
            let mut minx = i32::MAX;
            let mut maxx = i32::MIN;
            let mut miny = i32::MAX;
            let mut maxy = i32::MIN;
            for (i, p) in rgba.chunks_exact(4).enumerate() {
                if p[3] == 0 {
                    continue;
                }
                let x = (i % w as usize) as i32;
                let y = (i / w as usize) as i32;
                if x < minx {
                    minx = x;
                }
                if x > maxx {
                    maxx = x;
                }
                if y < miny {
                    miny = y;
                }
                if y > maxy {
                    maxy = y;
                }
            }
            (maxx - minx, maxy - miny)
        };
        let (uw, uh) = painted_extents(&up_rgba);
        let (rw, rh) = painted_extents(&rot_rgba);
        // upright run is wide and short; rotated run is tall and narrow.
        assert!(uw > uh, "upright label not horizontally extended: {uw}x{uh}");
        assert!(rh > rw, "rotated label not vertically extended: {rw}x{rh}");
    }

    #[test]
    fn style_opacity_halves_fill_alpha() {
        // opaque red square at opacity 0.5 on a transparent canvas: every
        // covered pixel should land near alpha 128, not 255. proves
        // style.opacity multiplies through to fill alpha.
        let canvas = Canvas {
            width: 32,
            height: 32,
            background: None,
        };
        let ops = vec![DrawOp::Path {
            path: square(16.0, 16.0, 8.0),
            style: Arc::new(Style {
                fill: Some(FillPaint::Solid(red())),
                opacity: Some(0.5),
                ..Default::default()
            }),
        }];
        let png_bytes = render_png(canvas, &ops);
        let (_, _, rgba) = decode(&png_bytes);
        // count pixels with the expected half-alpha. tolerate +/- 4 LSB for
        // edge anti-aliasing.
        let half = rgba
            .chunks_exact(4)
            .filter(|p| p[3] > 100 && p[3] < 160 && p[0] > 100)
            .count();
        assert!(half > 100, "expected half-alpha pixels, got {half}");
        // no fully-opaque red.
        let full = rgba.chunks_exact(4).filter(|p| p[3] >= 250).count();
        assert_eq!(full, 0, "opacity didn't gate fill alpha");
    }

    #[test]
    fn style_opacity_zero_paints_nothing() {
        let canvas = Canvas {
            width: 16,
            height: 16,
            background: None,
        };
        let ops = vec![DrawOp::Path {
            path: square(8.0, 8.0, 4.0),
            style: Arc::new(Style {
                fill: Some(FillPaint::Solid(red())),
                stroke: Some(red()),
                stroke_width: Some(1.0),
                opacity: Some(0.0),
                ..Default::default()
            }),
        }];
        let (_, _, rgba) = decode(&render_png(canvas, &ops));
        let painted = rgba.chunks_exact(4).filter(|p| p[3] > 0).count();
        assert_eq!(painted, 0, "opacity=0 should produce a fully transparent result");
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

//! CPU rasteriser. SPEC §11.2 - tiny-skia rasterisation, PNG encoding.
//! Label rendering and JPEG encoding deferred to later phases.

#![forbid(unsafe_code)]

mod encode;
mod raster;

use mars_render_port::{
    Canvas, DrawOp, EncodeError, Encoder, ImageFormat, Pixmap, RenderError, Renderer,
};
use tiny_skia::Pixmap as SkPixmap;

#[derive(Debug, Default)]
pub struct TinySkiaRenderer;

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

        let mut warned_label = false;
        for op in ops {
            match op {
                DrawOp::Path { path, style } => raster::draw_path(&mut pm, path, style),
                DrawOp::Label { .. } => {
                    if !warned_label {
                        tracing::warn!("phase-2: label rendering deferred");
                        warned_label = true;
                    }
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

/// PNG encoder; JPEG deferred to Phase 1. Lives next to the rasteriser so the
/// in-process pipeline does not pay an extra copy crossing crate boundaries.
#[derive(Debug, Default)]
pub struct TinySkiaEncoder;

impl Encoder for TinySkiaEncoder {
    fn encode(&self, pixmap: &Pixmap, format: ImageFormat) -> Result<Vec<u8>, EncodeError> {
        match format {
            ImageFormat::Png => encode::encode_png(pixmap),
            ImageFormat::Jpeg => Err(EncodeError::NotImplemented {
                what: "jpeg encoding deferred to Phase 1",
            }),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use mars_render_port::Path as PortPath;
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
            rings: vec![vec![
                (cx - half, cy - half),
                (cx + half, cy - half),
                (cx + half, cy + half),
                (cx - half, cy + half),
            ]],
        }
    }

    fn render_png(canvas: Canvas, ops: &[DrawOp]) -> Vec<u8> {
        let pm = TinySkiaRenderer.render(canvas, ops).unwrap();
        TinySkiaEncoder.encode(&pm, ImageFormat::Png).unwrap()
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
            rings: vec![vec![(8.0, 32.0), (56.0, 32.0)]],
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
            style_ref: "x".into(),
        }];
        let pm = TinySkiaRenderer.render(canvas, &ops);
        assert!(pm.is_ok(), "label op should be skipped, not error: {pm:?}");
    }

    #[test]
    fn jpeg_returns_not_implemented() {
        let canvas = Canvas {
            width: 4,
            height: 4,
            background: None,
        };
        let pm = TinySkiaRenderer.render(canvas, &[]).unwrap();
        let res = TinySkiaEncoder.encode(&pm, ImageFormat::Jpeg);
        assert!(matches!(res, Err(EncodeError::NotImplemented { .. })));
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

        let golden_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/square.png");
        if std::env::var("MARS_UPDATE_GOLDEN").is_ok() || !golden_path.exists() {
            std::fs::create_dir_all(golden_path.parent().unwrap()).unwrap();
            std::fs::write(&golden_path, &actual).unwrap();
        }
        let expected = std::fs::read(&golden_path).unwrap();
        assert_eq!(
            actual, expected,
            "golden mismatch; rerun with MARS_UPDATE_GOLDEN=1 if intentional"
        );
    }
}

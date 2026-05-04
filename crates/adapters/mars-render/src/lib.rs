//! CPU rasteriser. SPEC §11.2 - tiny-skia rasterisation, PNG encoding.
//! Label rendering and JPEG encoding deferred to later phases.

#![forbid(unsafe_code)]

mod encode;
mod raster;

use async_trait::async_trait;
use mars_render_port::{Canvas, DrawOp, ImageFormat, RenderError, Renderer};
use tiny_skia::Pixmap;

#[derive(Debug, Default)]
pub struct TinySkiaRenderer;

#[async_trait]
impl Renderer for TinySkiaRenderer {
    async fn render(&self, canvas: Canvas, ops: &[DrawOp], format: ImageFormat) -> Result<Vec<u8>, RenderError> {
        if !matches!(format, ImageFormat::Png) {
            return Err(RenderError::NotImplemented {
                what: "jpeg encoding deferred to Phase 1",
            });
        }
        if canvas.width == 0 || canvas.height == 0 {
            return Err(RenderError::Backend("canvas has zero dimension".into()));
        }

        let mut pm = Pixmap::new(canvas.width, canvas.height)
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

        encode::encode_png(&pm)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
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

    fn decode(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let dec = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = dec.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        buf.truncate(info.buffer_size());
        (info.width, info.height, buf)
    }

    #[tokio::test]
    async fn determinism_byte_exact() {
        let r = TinySkiaRenderer;
        let canvas = Canvas {
            width: 64,
            height: 64,
            background: Some(white()),
        };
        let ops = vec![DrawOp::Path {
            path: square(32.0, 32.0, 16.0),
            style: Style {
                fill: Some(red()),
                ..Default::default()
            },
        }];
        let a = r.render(canvas, &ops, ImageFormat::Png).await.unwrap();
        let b = r.render(canvas, &ops, ImageFormat::Png).await.unwrap();
        assert_eq!(a, b, "renderer must be deterministic");
    }

    #[tokio::test]
    async fn filled_polygon_red_pixels() {
        let r = TinySkiaRenderer;
        let canvas = Canvas {
            width: 64,
            height: 64,
            background: None,
        };
        let ops = vec![DrawOp::Path {
            path: square(32.0, 32.0, 16.0),
            style: Style {
                fill: Some(red()),
                ..Default::default()
            },
        }];
        let png_bytes = r.render(canvas, &ops, ImageFormat::Png).await.unwrap();
        let (_, _, rgba) = decode(&png_bytes);
        let red_count = rgba
            .chunks_exact(4)
            .filter(|p| p[0] > 200 && p[1] < 40 && p[2] < 40 && p[3] == 255)
            .count();
        assert!(red_count > 800, "expected >800 red pixels, got {red_count}");
    }

    #[tokio::test]
    async fn stroked_line_has_pixels() {
        let r = TinySkiaRenderer;
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
            style: Style {
                stroke: Some(red()),
                stroke_width: Some(2.0),
                ..Default::default()
            },
        }];
        let png_bytes = r.render(canvas, &ops, ImageFormat::Png).await.unwrap();
        let (w, _, rgba) = decode(&png_bytes);
        let row = 32usize * w as usize * 4;
        let on_row: usize = rgba[row..row + w as usize * 4]
            .chunks_exact(4)
            .filter(|p| p[3] > 0)
            .count();
        assert!(on_row >= 40, "expected stroked pixels on row 32, got {on_row}");
    }

    #[tokio::test]
    async fn transparent_vs_opaque_background() {
        let r = TinySkiaRenderer;

        let c1 = Canvas {
            width: 4,
            height: 4,
            background: None,
        };
        let png1 = r.render(c1, &[], ImageFormat::Png).await.unwrap();
        let (_, _, rgba1) = decode(&png1);
        assert_eq!(rgba1[3], 0, "transparent bg → first pixel alpha 0");

        let c2 = Canvas {
            width: 4,
            height: 4,
            background: Some(white()),
        };
        let png2 = r.render(c2, &[], ImageFormat::Png).await.unwrap();
        let (_, _, rgba2) = decode(&png2);
        assert_eq!(rgba2[3], 255, "opaque bg → first pixel alpha 255");
        assert_eq!(&rgba2[0..3], &[255, 255, 255]);
    }

    #[tokio::test]
    async fn label_op_is_skipped_not_errored() {
        let r = TinySkiaRenderer;
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
        let res = r.render(canvas, &ops, ImageFormat::Png).await;
        assert!(res.is_ok(), "label op should be skipped, not error: {res:?}");
    }

    #[tokio::test]
    async fn jpeg_returns_not_implemented() {
        let r = TinySkiaRenderer;
        let canvas = Canvas {
            width: 4,
            height: 4,
            background: None,
        };
        let res = r.render(canvas, &[], ImageFormat::Jpeg).await;
        assert!(matches!(res, Err(RenderError::NotImplemented { .. })));
    }

    #[tokio::test]
    async fn golden_square_matches() {
        let r = TinySkiaRenderer;
        let canvas = Canvas {
            width: 64,
            height: 64,
            background: Some(white()),
        };
        let ops = vec![DrawOp::Path {
            path: square(32.0, 32.0, 16.0),
            style: Style {
                fill: Some(red()),
                ..Default::default()
            },
        }];
        let actual = r.render(canvas, &ops, ImageFormat::Png).await.unwrap();

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

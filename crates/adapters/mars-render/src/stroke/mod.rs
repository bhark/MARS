//! polyline stroke pipeline.
//!
//! takes a `ResolvedStroke` (already opacity-folded and sub-pixel-clamped)
//! and emits a single tiny-skia stroke pass. supports parallel-offset via
//! `path_offset` when `offset_px != 0`. when `stroke_gap` is set, the
//! `gap` submodule stamps the parent style's marker along the line.

pub(crate) mod dash;
pub(crate) mod gap;

use mars_render_port::Path as PortPath;
use tiny_skia::{Paint, Pixmap, Stroke, Transform};

use crate::canvas::scaled_alpha;
use crate::path::build_path;
use crate::path_offset::offset_polyline;
use crate::prepare::ResolvedStroke;

pub(crate) fn draw(pm: &mut Pixmap, port_path: &PortPath, tsk_path: &tiny_skia::Path, stroke: &ResolvedStroke) {
    let mut paint = Paint::default();
    paint.set_color(scaled_alpha(stroke.colour, stroke.alpha));
    paint.anti_alias = true;

    let offset_path = if stroke.offset_px != 0.0 {
        offset_polyline(port_path, stroke.offset_px).and_then(|p| build_path(&p))
    } else {
        None
    };
    let tsk_stroke = Stroke {
        width: stroke.width,
        line_cap: stroke.cap,
        line_join: stroke.join,
        dash: stroke.dash.clone(),
        ..Stroke::default()
    };
    let path = offset_path.as_ref().unwrap_or(tsk_path);
    pm.stroke_path(path, &paint, &tsk_stroke, Transform::identity(), None);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use mars_render_port::{Canvas, DrawOp, Encoder, ImageFormat, Path as PortPath, Renderer, Subpath};
    use mars_style::{Colour, FillPaint, Style};

    use crate::{TinySkiaEncoder, TinySkiaRenderer};

    fn red() -> Colour {
        Colour::rgba(255, 0, 0, 255)
    }

    fn white() -> Colour {
        Colour::rgba(255, 255, 255, 255)
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
        let pm = TinySkiaRenderer::new(Arc::new(mars_text::Fonts::with_default()))
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
            style: Arc::new(
                Style {
                    stroke: Some(red()),
                    stroke_width: Some(2.0.into()),
                    ..Default::default()
                }
                .resolve(0),
            ),
        }];
        let (w, _, rgba) = decode(&render_png(canvas, &ops));
        let row = 32usize * w as usize * 4;
        let on_row: usize = rgba[row..row + w as usize * 4]
            .chunks_exact(4)
            .filter(|p| p[3] > 0)
            .count();
        assert!(on_row >= 40, "expected stroked pixels on row 32, got {on_row}");
    }

    #[test]
    fn open_linestring_does_not_close() {
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
            style: Arc::new(
                Style {
                    stroke: Some(red()),
                    stroke_width: Some(1.0.into()),
                    ..Default::default()
                }
                .resolve(0),
            ),
        }];
        let (w, _, rgba) = decode(&render_png(canvas, &ops));
        let br = ((w - 2) * 4) as usize + ((w * (canvas.height - 2)) * 4) as usize;
        assert_eq!(rgba[br + 3], 0, "open linestring must not close to bottom-right");
    }

    #[test]
    fn subpixel_stroke_0_15_is_visible_and_soft() {
        // a 0.15 px stroke must be visible (regression: tiny-skia drops thin
        // strokes outright) AND must be soft, not full-intensity. AGG-style
        // emulation = 1px stroke at proportional alpha.
        let canvas = Canvas {
            width: 64,
            height: 64,
            background: Some(white()),
        };
        let ops = vec![DrawOp::Path {
            path: square(32.0, 32.0, 16.0),
            style: Arc::new(
                Style {
                    fill: Some(FillPaint::Solid(Colour::rgb(220, 240, 255))),
                    stroke: Some(Colour::rgb(40, 150, 230)),
                    stroke_width: Some(0.15.into()),
                    ..Default::default()
                }
                .resolve(0),
            ),
        }];
        let (_, _, rgba) = decode(&render_png(canvas, &ops));

        let stroke_tinted = rgba.chunks_exact(4).filter(|p| p[0] < 220 && p[3] > 0).count();
        assert!(stroke_tinted > 0, "sub-pixel stroke produced no visible tint");

        let saturated = rgba.chunks_exact(4).filter(|p| p[0] < 120).count();
        assert_eq!(
            saturated, 0,
            "sub-pixel stroke is rendering at full intensity ({saturated} saturated px)"
        );
    }

    #[test]
    fn stroke_offset_shifts_line_perpendicular() {
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
            style: Arc::new(
                Style {
                    stroke: Some(red()),
                    stroke_width: Some(2.0.into()),
                    stroke_offset_px: Some(8.0),
                    ..Default::default()
                }
                .resolve(0),
            ),
        }];
        let (w, _, rgba) = decode(&render_png(canvas, &ops));
        let row_has_pixels = |y: usize| {
            let off = y * w as usize * 4;
            rgba[off..off + w as usize * 4]
                .chunks_exact(4)
                .any(|p| p[3] > 0 && p[0] > 150)
        };
        assert!(!row_has_pixels(32), "stroke wasn't displaced from y=32");
        assert!(row_has_pixels(40), "stroke didn't land at y=40");
    }
}

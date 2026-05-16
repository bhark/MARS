//! solid-colour polygon fill.

use mars_style::Colour;
use tiny_skia::{FillRule, Paint, Pixmap, Transform};

use crate::canvas::{colour_to_tsk, scaled_alpha_colour};

pub(crate) fn draw(pm: &mut Pixmap, path: &tiny_skia::Path, c: Colour, alpha: f32) {
    let colour = if alpha >= 1.0 { c } else { scaled_alpha_colour(c, alpha) };
    let mut paint = Paint::default();
    paint.set_color(colour_to_tsk(colour));
    paint.anti_alias = true;
    pm.fill_path(path, &paint, FillRule::EvenOdd, Transform::identity(), None);
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
    fn filled_polygon_red_pixels() {
        let canvas = Canvas {
            width: 64,
            height: 64,
            background: None,
        };
        let ops = vec![DrawOp::Path {
            path: square(32.0, 32.0, 16.0),
            style: Arc::new(
                Style {
                    fill: Some(FillPaint::Solid(red())),
                    ..Default::default()
                }
                .resolve(0),
            ),
        }];
        let (_, _, rgba) = decode(&render_png(canvas, &ops));
        let red_count = rgba
            .chunks_exact(4)
            .filter(|p| p[0] > 200 && p[1] < 40 && p[2] < 40 && p[3] == 255)
            .count();
        assert!(red_count > 800, "expected >800 red pixels, got {red_count}");
    }

    #[test]
    fn collapsed_polygon_fill_is_silently_skipped() {
        // closed ring whose vertices all share the same y; the is_fillable
        // gate prevents tiny-skia's fill_path from logging a warn.
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
            style: Arc::new(
                Style {
                    fill: Some(FillPaint::Solid(red())),
                    ..Default::default()
                }
                .resolve(0),
            ),
        }];
        let (_, _, rgba) = decode(&render_png(canvas, &ops));
        let opaque = rgba.chunks_exact(4).filter(|p| p[3] != 0).count();
        assert_eq!(opaque, 0, "degenerate-bbox fill must paint nothing");
    }

    #[test]
    fn style_opacity_halves_fill_alpha() {
        let canvas = Canvas {
            width: 32,
            height: 32,
            background: None,
        };
        let ops = vec![DrawOp::Path {
            path: square(16.0, 16.0, 8.0),
            style: Arc::new(
                Style {
                    fill: Some(FillPaint::Solid(red())),
                    opacity: Some(0.5),
                    ..Default::default()
                }
                .resolve(0),
            ),
        }];
        let (_, _, rgba) = decode(&render_png(canvas, &ops));
        let half = rgba
            .chunks_exact(4)
            .filter(|p| p[3] > 100 && p[3] < 160 && p[0] > 100)
            .count();
        assert!(half > 100, "expected half-alpha pixels, got {half}");
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
            style: Arc::new(
                Style {
                    fill: Some(FillPaint::Solid(red())),
                    stroke: Some(red()),
                    stroke_width: Some(1.0.into()),
                    opacity: Some(0.0),
                    ..Default::default()
                }
                .resolve(0),
            ),
        }];
        let (_, _, rgba) = decode(&render_png(canvas, &ops));
        let painted = rgba.chunks_exact(4).filter(|p| p[3] > 0).count();
        assert_eq!(painted, 0, "opacity=0 should produce a fully transparent result");
    }
}

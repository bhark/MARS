//! procedural parallel-line hatch fill.
//!
//! builds a polygon clip mask from the path, then stamps strokes oriented at
//! `angle_deg` (0 = horizontal, 90 = vertical) at `spacing` intervals across
//! the path's bbox.
//!
//! degenerate inputs (non-finite or non-positive numerics) silently produce no
//! fill - config-load validation rejects these before they reach here.
//!
//! perf: per-polygon cost is one full-canvas Mask allocation + scan-line
//! rasterisation of the polygon into the mask + stroke-path along
//! `bbox_extent / spacing` lines. on 1024x1024 canvases this measures ~6-7x
//! slower than `FillPaint::Solid` (benches/hatch.rs). a future optimisation:
//! pre-render one period of the hatch into a small tileable pixmap and stamp
//! it under the mask, trading the per-polygon stroke ops for a single textured
//! fill. landed only if hatch turns up in a hot cadastral tile path; the
//! current cost is acceptable for beta.

use mars_style::Colour;
use tiny_skia::{FillRule, LineCap, LineJoin, Mask, Paint, PathBuilder, Pixmap, Stroke, Transform};

use crate::canvas::{colour_to_tsk, scaled_alpha_colour};

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw(
    pm: &mut Pixmap,
    path: &tiny_skia::Path,
    spacing: f32,
    angle_deg: f32,
    line_width: f32,
    colour: Colour,
    alpha: f32,
) {
    if !(spacing.is_finite() && spacing > 0.0 && line_width.is_finite() && line_width > 0.0 && angle_deg.is_finite()) {
        return;
    }

    let Some(mut mask) = Mask::new(pm.width(), pm.height()) else {
        return;
    };
    mask.fill_path(path, FillRule::EvenOdd, true, Transform::identity());

    // strokes are emitted in the path's local frame and oriented by
    // (cos, sin) of the requested angle. the bbox of a path rotated by
    // -angle determines how many parallel strokes we need and their span.
    let theta = angle_deg.to_radians();
    let (sin_t, cos_t) = theta.sin_cos();
    let b = path.bounds();
    let corners = [
        (b.left(), b.top()),
        (b.right(), b.top()),
        (b.right(), b.bottom()),
        (b.left(), b.bottom()),
    ];
    // project corners onto the hatch-normal axis (perpendicular to stroke
    // direction). min/max give the range of perpendicular offsets we must
    // span; project onto the parallel axis to size the stroke length.
    let (nx, ny) = (-sin_t, cos_t); // normal axis (unit)
    let (tx, ty) = (cos_t, sin_t); // tangent axis (unit)
    let mut nmin = f32::INFINITY;
    let mut nmax = f32::NEG_INFINITY;
    let mut tmin = f32::INFINITY;
    let mut tmax = f32::NEG_INFINITY;
    for (cx, cy) in corners {
        let n = cx * nx + cy * ny;
        let t = cx * tx + cy * ty;
        if n < nmin {
            nmin = n;
        }
        if n > nmax {
            nmax = n;
        }
        if t < tmin {
            tmin = t;
        }
        if t > tmax {
            tmax = t;
        }
    }
    // pad by half a line-width so strokes at the edge are not clipped at
    // their bbox boundary.
    let pad = (line_width * 0.5).max(1.0);
    tmin -= pad;
    tmax += pad;

    let mut pb = PathBuilder::new();
    let mut n = nmin;
    let max_strokes = ((nmax - nmin) / spacing).ceil() as i32 + 2;
    let mut steps = 0;
    while n <= nmax && steps < max_strokes.max(1) {
        let (ax, ay) = (tmin * tx + n * nx, tmin * ty + n * ny);
        let (bx, by) = (tmax * tx + n * nx, tmax * ty + n * ny);
        pb.move_to(ax, ay);
        pb.line_to(bx, by);
        n += spacing;
        steps += 1;
    }
    let Some(stroke_path) = pb.finish() else {
        return;
    };

    let line_colour = if alpha >= 1.0 {
        colour
    } else {
        scaled_alpha_colour(colour, alpha)
    };
    let mut paint = Paint::default();
    paint.set_color(colour_to_tsk(line_colour));
    paint.anti_alias = true;
    let stroke = Stroke {
        width: line_width,
        line_cap: LineCap::Butt,
        line_join: LineJoin::Miter,
        ..Stroke::default()
    };
    pm.stroke_path(&stroke_path, &paint, &stroke, Transform::identity(), Some(&mask));
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use mars_render_port::{Canvas, DrawOp, Encoder, ImageFormat, Path as PortPath, Renderer, Subpath};
    use mars_style::{Colour, FillPaint, Style};

    use crate::{TinySkiaEncoder, TinySkiaRenderer};

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
    fn hatch_fill_paints_lines_inside_polygon() {
        let canvas = Canvas {
            width: 64,
            height: 64,
            background: None,
        };
        let ops = vec![DrawOp::Path {
            path: square(32.0, 32.0, 12.0),
            style: Arc::new(
                Style {
                    fill: Some(FillPaint::Hatch {
                        spacing: 4.0,
                        angle_deg: 0.0,
                        line_width: 1.0,
                        colour: Colour::rgba(220, 30, 30, 255),
                    }),
                    ..Default::default()
                }
                .resolve(0),
            ),
        }];
        let (w, _, rgba) = decode(&render_png(canvas, &ops));
        let is_red = |p: &[u8]| p[0] > 150 && p[1] < 80 && p[2] < 80 && p[3] > 0;

        let inside_red = rgba
            .chunks_exact(4)
            .enumerate()
            .filter(|(i, p)| {
                let x = (i % w as usize) as f32;
                let y = (i / w as usize) as f32;
                is_red(p) && (20.0..44.0).contains(&x) && (20.0..44.0).contains(&y)
            })
            .count();
        assert!(
            inside_red > 100,
            "hatch produced too few inside-poly red pixels: {inside_red}"
        );

        let outside_red = rgba
            .chunks_exact(4)
            .enumerate()
            .filter(|(i, p)| {
                let x = (i % w as usize) as f32;
                let y = (i / w as usize) as f32;
                is_red(p) && !((20.0..44.0).contains(&x) && (20.0..44.0).contains(&y))
            })
            .count();
        assert_eq!(
            outside_red, 0,
            "hatch leaked outside polygon clip mask: {outside_red} px"
        );
    }

    #[test]
    fn hatch_fill_at_45_degrees_paints_inside_polygon() {
        let canvas = Canvas {
            width: 64,
            height: 64,
            background: None,
        };
        let ops = vec![DrawOp::Path {
            path: square(32.0, 32.0, 12.0),
            style: Arc::new(
                Style {
                    fill: Some(FillPaint::Hatch {
                        spacing: 4.0,
                        angle_deg: 45.0,
                        line_width: 1.0,
                        colour: Colour::rgba(220, 30, 30, 255),
                    }),
                    ..Default::default()
                }
                .resolve(0),
            ),
        }];
        let (_, _, rgba) = decode(&render_png(canvas, &ops));
        let red_count = rgba
            .chunks_exact(4)
            .filter(|p| p[0] > 150 && p[1] < 80 && p[2] < 80 && p[3] > 0)
            .count();
        assert!(
            red_count > 100,
            "diagonal hatch produced too few red pixels: {red_count}"
        );
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
            style: Arc::new(
                Style {
                    fill: Some(FillPaint::Hatch {
                        spacing: 6.0,
                        angle_deg: 30.0,
                        line_width: 1.0,
                        colour: Colour::rgba(220, 30, 30, 255),
                    }),
                    stroke: Some(Colour::rgba(0, 200, 0, 255)),
                    stroke_width: Some(2.0.into()),
                    ..Default::default()
                }
                .resolve(0),
            ),
        }];
        let (_, _, rgba) = decode(&render_png(canvas, &ops));
        let green_count = rgba
            .chunks_exact(4)
            .filter(|p| p[1] > 150 && p[0] < 80 && p[2] < 80 && p[3] > 0)
            .count();
        assert!(green_count > 50, "outline stroke missing: only {green_count} green px");
    }
}

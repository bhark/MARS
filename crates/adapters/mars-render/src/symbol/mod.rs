//! point-marker dispatch hub. mirrors the variant-per-file shape of
//! `fill/` and `stroke/`: each `MarkerShape` variant lives in a sibling
//! module and is reached through a single exhaustive match. adding a
//! variant in `mars-style` breaks the build here, forcing the conversation
//! about whether the new marker is wired or staged.
//!
//! A `None` marker on a `Symbol` op is a runtime contract slip; rendering
//! no-ops rather than aborting the batch, consistent with how empty paths
//! and zero-width strokes are tolerated elsewhere in the pipeline.

mod circle;
mod cross;
mod glyph;
mod pin;
mod square;
mod triangle;
mod vector_shape;
mod x;

use mars_render_port::{Path as PortPath, RenderError};
use mars_style::{MarkerShape, ResolvedStyle};
use mars_text::Fonts;
use tiny_skia::Pixmap;

pub(crate) fn dispatch(
    pm: &mut Pixmap,
    anchor: (f32, f32),
    rotation_rad: f32,
    style: &ResolvedStyle,
    fonts: &Fonts,
) -> Result<(), RenderError> {
    let Some(marker) = &style.marker else {
        return Ok(());
    };
    let size = marker.size;
    match &marker.shape {
        MarkerShape::Glyph { font_family, ch } => {
            glyph::draw(pm, anchor, rotation_rad, font_family, ch, size, style, fonts)
        }
        MarkerShape::Circle => render(pm, circle::build_path(size), anchor, rotation_rad, style, fonts),
        MarkerShape::Square => render(pm, square::build_path(size), anchor, rotation_rad, style, fonts),
        MarkerShape::Triangle => render(pm, triangle::build_path(size), anchor, rotation_rad, style, fonts),
        MarkerShape::Cross => render(pm, cross::build_path(size), anchor, rotation_rad, style, fonts),
        MarkerShape::X => render(pm, x::build_path(size), anchor, rotation_rad, style, fonts),
        MarkerShape::Pin => render(pm, pin::build_path(size), anchor, rotation_rad, style, fonts),
        MarkerShape::VectorShape {
            points,
            anchor: local_anchor,
            filled,
        } => {
            let path = vector_shape::build_path(points, *local_anchor, *filled, size);
            if *filled {
                render(pm, path, anchor, rotation_rad, style, fonts)
            } else {
                // open polyline: clear fill so the polygon pipeline is bypassed.
                // a fill paint on an open path would be auto-closed by
                // tiny-skia, which is the wrong semantics.
                let mut s = style.clone();
                s.fill = None;
                render(pm, path, anchor, rotation_rad, &s, fonts)
            }
        }
    }
}

// rotate each subpath point around the local origin by `rotation_rad`,
// then translate by `anchor`, and delegate to the path pipeline so fill
// and stroke flow through the same prepare / resolve / draw chain that
// DrawOp::Path uses.
fn render(
    pm: &mut Pixmap,
    mut local: PortPath,
    anchor: (f32, f32),
    rotation_rad: f32,
    style: &ResolvedStyle,
    fonts: &Fonts,
) -> Result<(), RenderError> {
    let (sin_r, cos_r) = rotation_rad.sin_cos();
    for sub in &mut local.subpaths {
        for p in &mut sub.points {
            let (x, y) = *p;
            *p = (anchor.0 + cos_r * x - sin_r * y, anchor.1 + sin_r * x + cos_r * y);
        }
    }
    crate::ops::path::draw(pm, &local, style, fonts)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use mars_render_port::{Canvas, DrawOp, Encoder, ImageFormat, Renderer};
    use mars_style::{Colour, FillPaint, MarkerSymbol, Style};
    use tiny_skia::Pixmap as SkPixmap;

    use super::*;
    use crate::{TinySkiaEncoder, TinySkiaRenderer};

    fn marker(shape: MarkerShape, size: f32) -> MarkerSymbol {
        MarkerSymbol {
            shape,
            size: size.into(),
            angle: None,
        }
    }

    fn pm() -> SkPixmap {
        SkPixmap::new(16, 16).unwrap()
    }

    fn render_marker(marker: MarkerSymbol) -> Vec<u8> {
        render_marker_at(marker, 32, 0.0)
    }

    fn render_marker_at(marker: MarkerSymbol, side: u32, rotation_rad: f32) -> Vec<u8> {
        let canvas = Canvas {
            width: side,
            height: side,
            background: None,
        };
        let style = Style {
            fill: Some(FillPaint::Solid(Colour {
                r: 255,
                g: 0,
                b: 0,
                a: 255,
            })),
            marker: Some(marker),
            ..Default::default()
        };
        let op = DrawOp::Symbol {
            anchor: (side as f32 / 2.0, side as f32 / 2.0),
            rotation_rad,
            style: Arc::new(style.resolve(0)),
        };
        let renderer = TinySkiaRenderer::new(Arc::new(mars_text::Fonts::with_default()));
        let pm = renderer.render(canvas, &[op]).expect("render ok");
        TinySkiaEncoder::default()
            .encode(&pm, ImageFormat::Png)
            .expect("encode ok")
    }

    fn red_pixel_count(png: &[u8]) -> usize {
        let dec = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = dec.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        buf.truncate(info.buffer_size());
        buf.chunks_exact(4)
            .filter(|p| p[0] > 200 && p[1] < 60 && p[2] < 60 && p[3] > 200)
            .count()
    }

    // (width, height) of the bbox of any non-transparent pixel.
    fn painted_extents(png: &[u8]) -> (i32, i32) {
        let dec = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = dec.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        buf.truncate(info.buffer_size());
        let w = info.width as usize;
        let mut minx = i32::MAX;
        let mut maxx = i32::MIN;
        let mut miny = i32::MAX;
        let mut maxy = i32::MIN;
        for (i, p) in buf.chunks_exact(4).enumerate() {
            if p[3] == 0 {
                continue;
            }
            let x = (i % w) as i32;
            let y = (i / w) as i32;
            minx = minx.min(x);
            maxx = maxx.max(x);
            miny = miny.min(y);
            maxy = maxy.max(y);
        }
        (maxx - minx, maxy - miny)
    }

    #[test]
    fn none_marker_is_silent_no_op() {
        let style = Style::default().resolve(0);
        let fonts = mars_text::Fonts::with_default();
        dispatch(&mut pm(), (8.0, 8.0), 0.0, &style, &fonts).expect("ok");
    }

    #[test]
    fn glyph_marker_paints_pixels_at_anchor() {
        let png = render_marker(marker(
            MarkerShape::Glyph {
                font_family: "DejaVu Sans".into(),
                ch: "A".into(),
            },
            18.0,
        ));
        let n = red_pixel_count(&png);
        // exact glyph coverage depends on the bundled font; loose bounds
        // confirm the glyph rasterises at the requested colour without
        // being blank or overflowing the canvas.
        assert!(n > 20 && n < 250, "expected non-trivial 'A' coverage, got {n}");
    }

    #[test]
    fn glyph_marker_rotation_changes_painted_aspect() {
        let upright = render_marker_at(
            marker(
                MarkerShape::Glyph {
                    font_family: "DejaVu Sans".into(),
                    ch: "I".into(),
                },
                18.0,
            ),
            32,
            0.0,
        );
        let rotated = render_marker_at(
            marker(
                MarkerShape::Glyph {
                    font_family: "DejaVu Sans".into(),
                    ch: "I".into(),
                },
                18.0,
            ),
            32,
            std::f32::consts::FRAC_PI_2,
        );
        let (uw, uh) = painted_extents(&upright);
        let (rw, rh) = painted_extents(&rotated);
        assert!(uh > uw, "upright 'I' must be taller than wide: {uw}x{uh}");
        assert!(rw > rh, "rotated 'I' must be wider than tall: {rw}x{rh}");
    }

    #[test]
    fn glyph_marker_empty_string_is_typed_error() {
        let style = Style {
            fill: Some(FillPaint::Solid(Colour::rgba(255, 0, 0, 255))),
            marker: Some(marker(
                MarkerShape::Glyph {
                    font_family: "DejaVu Sans".into(),
                    ch: String::new(),
                },
                12.0,
            )),
            ..Default::default()
        }
        .resolve(0);
        let fonts = mars_text::Fonts::with_default();
        let err = dispatch(&mut pm(), (8.0, 8.0), 0.0, &style, &fonts).expect_err("must error");
        assert!(matches!(err, RenderError::Backend(msg) if msg.contains("empty ch")));
    }

    #[test]
    fn glyph_marker_rejects_non_solid_fill() {
        let style = Style {
            fill: Some(FillPaint::Image { name: "brick".into() }),
            marker: Some(marker(
                MarkerShape::Glyph {
                    font_family: "DejaVu Sans".into(),
                    ch: "A".into(),
                },
                12.0,
            )),
            ..Default::default()
        }
        .resolve(0);
        let fonts = mars_text::Fonts::with_default();
        let err = dispatch(&mut pm(), (8.0, 8.0), 0.0, &style, &fonts).expect_err("must error");
        assert!(matches!(err, RenderError::Backend(msg) if msg.contains("solid fill")));
    }

    #[test]
    fn circle_marker_paints_red_pixels() {
        let png = render_marker(marker(MarkerShape::Circle, 12.0));
        // 12px diameter circle = pi * 6^2 ≈ 113 covered pixels; allow slack
        // for antialiased edge softening and tiny-skia coverage rounding.
        let n = red_pixel_count(&png);
        assert!(
            n > 90 && n < 140,
            "expected ~113 fully-red pixels for a 12px circle, got {n}"
        );
    }

    #[test]
    fn square_marker_paints_red_pixels() {
        let png = render_marker(marker(MarkerShape::Square, 10.0));
        // 10x10 square = 100 fully covered pixels; antialiased edges round
        // slightly under, so allow some slack.
        let n = red_pixel_count(&png);
        assert!(
            n > 80 && n < 110,
            "expected ~100 fully-red pixels for a 10px square, got {n}"
        );
    }

    #[test]
    fn triangle_marker_paints_red_pixels() {
        let png = render_marker(marker(MarkerShape::Triangle, 12.0));
        // equilateral triangle with base 12 has area ~= 12^2 * sqrt(3)/4
        // ≈ 62; allow generous slack for antialiased apex/edge softening.
        let n = red_pixel_count(&png);
        assert!(
            n > 40 && n < 85,
            "expected ~62 fully-red pixels for a 12px triangle, got {n}"
        );
    }

    #[test]
    fn cross_marker_paints_red_pixels() {
        let png = render_marker(marker(MarkerShape::Cross, 12.0));
        // + sign with arm length 12 and thickness 4 covers a 12x4 bar plus
        // a 4x12 bar minus the shared 4x4 centre = 48 + 48 - 16 = 80.
        let n = red_pixel_count(&png);
        assert!(
            n > 60 && n < 100,
            "expected ~80 fully-red pixels for a 12px cross, got {n}"
        );
    }

    #[test]
    fn x_marker_paints_red_pixels() {
        let png = render_marker(marker(MarkerShape::X, 12.0));
        // same coverage as cross (it's the same polygon rotated 45°).
        let n = red_pixel_count(&png);
        assert!(n > 60 && n < 100, "expected ~80 fully-red pixels for a 12px x, got {n}");
    }

    #[test]
    fn pin_marker_paints_red_pixels() {
        let png = render_marker(marker(MarkerShape::Pin, 12.0));
        // bulb area = pi*36 ≈ 113 + tail triangle from tangents to apex
        // ≈ 47, minus overlap; allow generous slack for arc + apex AA.
        let n = red_pixel_count(&png);
        assert!(
            n > 120 && n < 200,
            "expected ~160 fully-red pixels for a 12px pin, got {n}"
        );
    }

    #[test]
    fn vector_shape_filled_paints_polygon_interior() {
        let png = render_marker(marker(
            MarkerShape::VectorShape {
                points: vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)],
                anchor: (0.5, 0.5),
                filled: true,
            },
            10.0,
        ));
        // unit-square scaled to 10px = ~100 fully-red pixels.
        let n = red_pixel_count(&png);
        assert!(
            n > 80 && n < 110,
            "expected ~100 fully-red pixels for a 10px vector-shape square, got {n}"
        );
    }
}

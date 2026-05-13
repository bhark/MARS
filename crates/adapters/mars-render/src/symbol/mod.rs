//! point-marker dispatch hub. mirrors the variant-per-file shape of
//! `fill/` and `stroke/`: each `MarkerSymbol` variant lives in a sibling
//! module and is reached through a single exhaustive match. adding a
//! variant in `mars-style` breaks the build here, forcing the conversation
//! about whether the new marker is wired or staged.
//!
//! `Glyph` continues to surface through `UnimplementedFeatures::glyph_marker`
//! (the existing flag); the warn-but-continue contract in the renderer
//! entry point already covers it. A `None` marker on a `Symbol` op is a
//! runtime contract slip; rendering no-ops rather than aborting the batch,
//! consistent with how empty paths and zero-width strokes are tolerated
//! elsewhere in the pipeline.

mod circle;
mod cross;
mod pin;
mod square;
mod triangle;
mod vector_shape;
mod x;

use mars_render_port::{Path as PortPath, RenderError};
use mars_style::{MarkerSymbol, Style};
use tiny_skia::Pixmap;

use crate::prepare::UnimplementedFeatures;

pub(crate) fn dispatch(
    pm: &mut Pixmap,
    anchor: (f32, f32),
    rotation_rad: f32,
    style: &Style,
) -> Result<UnimplementedFeatures, RenderError> {
    let Some(marker) = &style.marker else {
        return Ok(UnimplementedFeatures::default());
    };
    match marker {
        MarkerSymbol::Glyph { .. } => Ok(UnimplementedFeatures {
            glyph_marker: true,
            ..Default::default()
        }),
        MarkerSymbol::Circle { size } => render(pm, circle::build_path(*size), anchor, rotation_rad, style),
        MarkerSymbol::Square { size } => render(pm, square::build_path(*size), anchor, rotation_rad, style),
        MarkerSymbol::Triangle { size } => render(pm, triangle::build_path(*size), anchor, rotation_rad, style),
        MarkerSymbol::Cross { size } => render(pm, cross::build_path(*size), anchor, rotation_rad, style),
        MarkerSymbol::X { size } => render(pm, x::build_path(*size), anchor, rotation_rad, style),
        MarkerSymbol::Pin { .. } => Err(RenderError::NotImplemented {
            what: "MarkerSymbol::Pin",
        }),
        MarkerSymbol::VectorShape { .. } => Err(RenderError::NotImplemented {
            what: "MarkerSymbol::VectorShape",
        }),
        // `#[non_exhaustive]` forward-compat: future variants land here
        // until they grow a sibling module + dispatch arm above.
        _ => Err(RenderError::NotImplemented {
            what: "unknown MarkerSymbol variant",
        }),
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
    style: &Style,
) -> Result<UnimplementedFeatures, RenderError> {
    let (sin_r, cos_r) = rotation_rad.sin_cos();
    for sub in &mut local.subpaths {
        for p in &mut sub.points {
            let (x, y) = *p;
            *p = (anchor.0 + cos_r * x - sin_r * y, anchor.1 + sin_r * x + cos_r * y);
        }
    }
    crate::ops::path::draw(pm, &local, style)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use mars_render_port::{Canvas, DrawOp, Encoder, ImageFormat, Renderer};
    use mars_style::{Colour, FillPaint, Style};
    use tiny_skia::Pixmap as SkPixmap;

    use super::*;
    use crate::{TinySkiaEncoder, TinySkiaRenderer};

    fn pm() -> SkPixmap {
        SkPixmap::new(16, 16).unwrap()
    }

    fn render_marker(marker: MarkerSymbol) -> Vec<u8> {
        let canvas = Canvas {
            width: 32,
            height: 32,
            background: None,
        };
        let op = DrawOp::Symbol {
            anchor: (16.0, 16.0),
            rotation_rad: 0.0,
            style: Arc::new(Style {
                fill: Some(FillPaint::Solid(Colour {
                    r: 255,
                    g: 0,
                    b: 0,
                    a: 255,
                })),
                marker: Some(marker),
                ..Default::default()
            }),
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

    #[test]
    fn none_marker_is_silent_no_op() {
        let style = Style::default();
        let flags = dispatch(&mut pm(), (8.0, 8.0), 0.0, &style).expect("ok");
        assert!(!flags.any(), "no-op must not flag");
    }

    #[test]
    fn glyph_marker_surfaces_flag_without_error() {
        let style = Style {
            marker: Some(MarkerSymbol::Glyph {
                font_family: "x".into(),
                ch: "a".into(),
                size: 6.0,
            }),
            ..Default::default()
        };
        let flags = dispatch(&mut pm(), (8.0, 8.0), 0.0, &style).expect("ok");
        assert!(flags.glyph_marker, "glyph_marker must propagate");
    }

    #[test]
    fn circle_marker_paints_red_pixels() {
        let png = render_marker(MarkerSymbol::Circle { size: 12.0 });
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
        let png = render_marker(MarkerSymbol::Square { size: 10.0 });
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
        let png = render_marker(MarkerSymbol::Triangle { size: 12.0 });
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
        let png = render_marker(MarkerSymbol::Cross { size: 12.0 });
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
        let png = render_marker(MarkerSymbol::X { size: 12.0 });
        // same coverage as cross (it's the same polygon rotated 45°).
        let n = red_pixel_count(&png);
        assert!(
            n > 60 && n < 100,
            "expected ~80 fully-red pixels for a 12px x, got {n}"
        );
    }
}

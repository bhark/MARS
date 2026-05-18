#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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

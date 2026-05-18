#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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
fn determinism_byte_exact() {
    let canvas = Canvas {
        width: 64,
        height: 64,
        background: Some(white()),
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
    let a = render_png(canvas, &ops);
    let b = render_png(canvas, &ops);
    assert_eq!(a, b, "renderer must be deterministic");
}

#[test]
fn encode_round_trip_preserves_dimensions() {
    let canvas = Canvas {
        width: 48,
        height: 32,
        background: Some(white()),
    };
    let ops = vec![DrawOp::Path {
        path: square(24.0, 16.0, 8.0),
        style: Arc::new(
            Style {
                fill: Some(FillPaint::Solid(red())),
                ..Default::default()
            }
            .resolve(0),
        ),
    }];
    let png = render_png(canvas, &ops);
    let (w, h, rgba) = decode(&png);
    assert_eq!((w, h), (48, 32), "decoded dims must match canvas");
    assert_eq!(rgba.len(), (w * h * 4) as usize, "rgba channel count");
}

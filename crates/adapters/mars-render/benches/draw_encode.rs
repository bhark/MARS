//! draw + encode cost for the tiny-skia adapter. exercised independently of
//! mars-runtime so renderer/encoder signal isn't masked by upstream work.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::hint::black_box;
use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mars_render::{TinySkiaEncoder, TinySkiaRenderer};
use mars_render_port::{Canvas, DrawOp, Encoder, ImageFormat, Path as PortPath, Pixmap, Renderer, Subpath};
use mars_style::{Colour, FillPaint, Style};

const CANVAS_W: u32 = 1024;
const CANVAS_H: u32 = 1024;

fn fill_red() -> Arc<Style> {
    Arc::new(Style {
        fill: Some(FillPaint::Solid(Colour::rgba(200, 60, 40, 255))),
        ..Default::default()
    })
}

fn stroke_blue() -> Arc<Style> {
    Arc::new(Style {
        stroke: Some(Colour::rgba(40, 80, 200, 255)),
        stroke_width: Some(1.5),
        ..Default::default()
    })
}

fn polygon_path(cx: f32, cy: f32, r: f32, sides: usize) -> PortPath {
    let mut points = Vec::with_capacity(sides);
    for i in 0..sides {
        let theta = (i as f32) * std::f32::consts::TAU / (sides as f32);
        points.push((cx + theta.cos() * r, cy + theta.sin() * r));
    }
    PortPath {
        subpaths: vec![Subpath { points, closed: true }],
    }
}

fn line_path(cx: f32, cy: f32, len: f32) -> PortPath {
    let mut points = Vec::with_capacity(64);
    for i in 0..64 {
        let t = i as f32 / 64.0;
        let x = cx + (t - 0.5) * len;
        let y = cy + (t * 12.0).sin() * 8.0;
        points.push((x, y));
    }
    PortPath {
        subpaths: vec![Subpath { points, closed: false }],
    }
}

fn polygon_ops(n: usize) -> Vec<DrawOp> {
    let style = fill_red();
    (0..n)
        .map(|i| {
            // pseudo-random scatter inside the canvas; sides=8 keeps cost
            // dominated by tiny-skia tessellation rather than path setup.
            let x = ((i as u32).wrapping_mul(2654435761) % CANVAS_W) as f32;
            let y = ((i as u32).wrapping_mul(40503) % CANVAS_H) as f32;
            DrawOp::Path {
                path: polygon_path(x, y, 8.0, 8),
                style: Arc::clone(&style),
            }
        })
        .collect()
}

fn line_ops(n: usize) -> Vec<DrawOp> {
    let style = stroke_blue();
    (0..n)
        .map(|i| {
            let x = ((i as u32).wrapping_mul(2654435761) % CANVAS_W) as f32;
            let y = ((i as u32).wrapping_mul(40503) % CANVAS_H) as f32;
            DrawOp::Path {
                path: line_path(x, y, 32.0),
                style: Arc::clone(&style),
            }
        })
        .collect()
}

fn bench_draw_polygons(c: &mut Criterion) {
    let renderer = TinySkiaRenderer::new(std::sync::Arc::new(mars_text::Fonts::with_default()));
    let canvas = Canvas {
        width: CANVAS_W,
        height: CANVAS_H,
        background: Some(Colour::rgba(255, 255, 255, 255)),
    };

    let mut group = c.benchmark_group("render_draw_polygons");
    for n in [256usize, 4096] {
        let ops = polygon_ops(n);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &ops, |b, ops| {
            b.iter(|| {
                let pm = renderer.render(canvas, ops).unwrap();
                black_box(pm.premultiplied_rgba.len())
            });
        });
    }
    group.finish();
}

fn bench_draw_lines(c: &mut Criterion) {
    let renderer = TinySkiaRenderer::new(std::sync::Arc::new(mars_text::Fonts::with_default()));
    let canvas = Canvas {
        width: CANVAS_W,
        height: CANVAS_H,
        background: None,
    };

    let mut group = c.benchmark_group("render_draw_lines");
    for n in [256usize, 2048] {
        let ops = line_ops(n);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &ops, |b, ops| {
            b.iter(|| {
                let pm = renderer.render(canvas, ops).unwrap();
                black_box(pm.premultiplied_rgba.len())
            });
        });
    }
    group.finish();
}

fn make_pixmap(w: u32, h: u32) -> Pixmap {
    // synthetic content: gradient + a filled circle so encoders can't
    // collapse to a trivial single-colour stream.
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let cx = w as f32 * 0.5;
    let cy = h as f32 * 0.5;
    let r2 = (w.min(h) as f32 * 0.4).powi(2);
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let inside = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)) < r2;
            let (r, g, b) = if inside {
                (220u8, 60, 50)
            } else {
                (((x * 255) / w.max(1)) as u8, ((y * 255) / h.max(1)) as u8, 128)
            };
            buf[i] = r;
            buf[i + 1] = g;
            buf[i + 2] = b;
            buf[i + 3] = 255;
        }
    }
    Pixmap {
        width: w,
        height: h,
        premultiplied_rgba: buf,
    }
}

fn bench_encode(c: &mut Criterion) {
    let encoder = TinySkiaEncoder::new(80, mars_render::PngCompression::Fast);

    let mut group = c.benchmark_group("render_encode");
    for (label, w, h) in [("256x256", 256u32, 256u32), ("1024x1024", 1024, 1024)] {
        let pm = make_pixmap(w, h);
        group.throughput(Throughput::Bytes((w * h * 4) as u64));
        group.bench_with_input(BenchmarkId::new("png", label), &pm, |b, pm| {
            b.iter(|| {
                let bytes = encoder.encode(pm, ImageFormat::Png).unwrap();
                black_box(bytes.len())
            });
        });
    }

    // jpeg cost scales with pixmap size; one larger probe is enough.
    let pm = make_pixmap(1024, 1024);
    group.throughput(Throughput::Bytes(1024 * 1024 * 4));
    group.bench_with_input(BenchmarkId::new("jpeg", "1024x1024_q80"), &pm, |b, pm| {
        b.iter(|| {
            let bytes = encoder.encode(pm, ImageFormat::Jpeg).unwrap();
            black_box(bytes.len())
        });
    });
    group.finish();
}

criterion_group!(benches, bench_draw_polygons, bench_draw_lines, bench_encode);
criterion_main!(benches);

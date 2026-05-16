//! per-polygon hatch fill cost vs solid fill baseline. validates the
//! pessimistic case for FillPaint::Hatch:
//!   - one alpha-mask allocation sized to the canvas,
//!   - O(bbox_extent / spacing) parallel strokes.
//!
//! Land the pixmap-stamp fallback if hatch exceeds the Solid baseline by
//! more than ~5x on representative cadastral fixtures at the typical tile
//! sizes (256x256 / 512x512).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use mars_render::TinySkiaRenderer;
use mars_render_port::{Canvas, DrawOp, Path as PortPath, Renderer, Subpath};
use mars_style::{Colour, FillPaint, ResolvedStyle, Style};

const CANVAS_W: u32 = 1024;
const CANVAS_H: u32 = 1024;

fn solid_style() -> Arc<ResolvedStyle> {
    Arc::new(
        Style {
            fill: Some(FillPaint::Solid(Colour::rgba(200, 60, 40, 255))),
            ..Default::default()
        }
        .resolve(0),
    )
}

fn hatch_style() -> Arc<ResolvedStyle> {
    Arc::new(
        Style {
            fill: Some(FillPaint::Hatch {
                spacing: 4.0,
                angle_deg: 45.0,
                line_width: 0.6,
                colour: Colour::rgba(40, 80, 200, 255),
            }),
            ..Default::default()
        }
        .resolve(0),
    )
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

fn build_ops(n: usize, style: Arc<ResolvedStyle>) -> Vec<DrawOp> {
    let cols = (n as f32).sqrt().ceil() as usize;
    let pad = 12.0;
    let extent = ((CANVAS_W as f32) - 2.0 * pad) / cols as f32;
    let r = extent * 0.45;
    let mut ops = Vec::with_capacity(n);
    for i in 0..n {
        let col = (i % cols) as f32;
        let row = (i / cols) as f32;
        let cx = pad + extent * 0.5 + col * extent;
        let cy = pad + extent * 0.5 + row * extent;
        let path = polygon_path(cx, cy, r, 6);
        ops.push(DrawOp::Path {
            path,
            style: style.clone(),
        });
    }
    ops
}

fn bench(c: &mut Criterion) {
    let fonts = std::sync::Arc::new(mars_text::Fonts::with_default());
    let renderer = TinySkiaRenderer::new(fonts);
    let canvas = Canvas {
        width: CANVAS_W,
        height: CANVAS_H,
        background: None,
    };

    let mut g = c.benchmark_group("render_fill_polygons");
    for n in [64usize, 256, 1024] {
        let solid_ops = build_ops(n, solid_style());
        let hatch_ops = build_ops(n, hatch_style());
        g.bench_function(format!("solid/{n}"), |b| {
            b.iter(|| {
                let _ = renderer.render(black_box(canvas), black_box(&solid_ops)).unwrap();
            })
        });
        g.bench_function(format!("hatch/{n}"), |b| {
            b.iter(|| {
                let _ = renderer.render(black_box(canvas), black_box(&hatch_ops)).unwrap();
            })
        });
    }
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use mars_render_port::{Canvas, DrawOp, Encoder, ImageFormat, Renderer};
use mars_style::{AnchorPosition, Colour, ResolvedLabelStyle};

use crate::{TinySkiaEncoder, TinySkiaRenderer};

fn style() -> Arc<ResolvedLabelStyle> {
    Arc::new(ResolvedLabelStyle {
        font_family: "DejaVu Sans".into(),
        font_size: 16.0,
        fill: Colour::rgba(0, 0, 0, 255),
        halo: None,
        priority: 0,
        min_distance: 0.0,
        position: AnchorPosition::default(),
        offset_px: (0.0, 0.0),
        angle_deg: None,
        partials: false,
        force: false,
    })
}

fn render_png(canvas: Canvas, ops: &[DrawOp]) -> Vec<u8> {
    let pm = TinySkiaRenderer::new(Arc::new(mars_text::Fonts::with_default()))
        .render(canvas, ops)
        .unwrap();
    TinySkiaEncoder::default().encode(&pm, ImageFormat::Png).unwrap()
}

#[test]
fn follow_label_paints_text_along_horizontal_polyline() {
    let canvas = Canvas {
        width: 128,
        height: 32,
        background: None,
    };
    let op = DrawOp::FollowLabel {
        polyline_px: vec![(8.0, 20.0), (120.0, 20.0)],
        start_arc_px: 16.0,
        text: "ABCD".into(),
        style: style(),
    };
    let png = render_png(canvas, &[op]);
    // any painted pixels at all is enough; the assertion below
    // verifies they land roughly along the polyline.
    let dec = png::Decoder::new(std::io::Cursor::new(&png));
    let mut reader = dec.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    buf.truncate(info.buffer_size());
    let lit = buf.chunks_exact(4).filter(|p| p[3] > 0).count();
    assert!(lit > 0, "FOLLOW must paint glyph pixels");
    // painted pixels should sit near y = 20 (the polyline). count any
    // pixel within ±15 px of the line.
    let near_line = buf
        .chunks_exact(4)
        .enumerate()
        .filter(|(_, p)| p[3] > 0)
        .filter(|(i, _)| {
            let y = (i / canvas.width as usize) as i32;
            (y - 20).abs() <= 15
        })
        .count();
    // tolerate antialias tails outside the band; just require the
    // majority lands near the line.
    assert!(near_line * 2 >= lit, "{near_line} of {lit} pixels near line");
}

#[test]
fn follow_label_skips_glyphs_whose_arc_falls_off_the_polyline() {
    // 16 px polyline + start_arc=0; only the first glyph (centre arc ≈
    // half-advance) fits. the rest land past arc_total and are dropped.
    let canvas = Canvas {
        width: 64,
        height: 32,
        background: None,
    };
    let op = DrawOp::FollowLabel {
        polyline_px: vec![(0.0, 16.0), (16.0, 16.0)],
        start_arc_px: 0.0,
        text: "ABCDEFGHIJ".into(),
        style: style(),
    };
    // smoke: must not error or panic even when many glyphs spill off
    // the end of the polyline.
    let _png = render_png(canvas, &[op]);
}

#[test]
fn follow_label_with_empty_text_paints_nothing() {
    let canvas = Canvas {
        width: 32,
        height: 32,
        background: None,
    };
    let op = DrawOp::FollowLabel {
        polyline_px: vec![(0.0, 16.0), (32.0, 16.0)],
        start_arc_px: 0.0,
        text: String::new(),
        style: style(),
    };
    let png = render_png(canvas, &[op]);
    let dec = png::Decoder::new(std::io::Cursor::new(&png));
    let mut reader = dec.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    buf.truncate(info.buffer_size());
    let lit = buf.chunks_exact(4).filter(|p| p[3] > 0).count();
    assert_eq!(lit, 0, "empty text must paint nothing");
}

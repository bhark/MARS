#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use mars_render_port::{Canvas, DrawOp, Encoder, ImageFormat, Renderer};
use mars_style::{AnchorPosition, Colour, ResolvedLabelStyle};

use crate::{TinySkiaEncoder, TinySkiaRenderer};

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
fn label_op_is_skipped_not_errored() {
    let canvas = Canvas {
        width: 8,
        height: 8,
        background: None,
    };
    let ops = vec![DrawOp::Label {
        anchor: (0.0, 0.0),
        text: "hi".into(),
        style: Arc::new(ResolvedLabelStyle {
            font_family: "DejaVu Sans".into(),
            font_size: 12.0,
            fill: Colour::rgba(0, 0, 0, 255),
            halo: None,
            priority: 0,
            min_distance: 0.0,
            position: AnchorPosition::default(),
            offset_px: (0.0, 0.0),
            angle_deg: None,
            partials: false,
            force: false,
        }),
        angle_rad: 0.0,
    }];
    let pm = TinySkiaRenderer::new(Arc::new(mars_text::Fonts::with_default())).render(canvas, &ops);
    assert!(pm.is_ok(), "label op should be skipped, not error: {pm:?}");
}

#[test]
fn rotated_label_lands_in_rotated_bbox() {
    let canvas = Canvas {
        width: 64,
        height: 64,
        background: None,
    };
    let label_style = Arc::new(ResolvedLabelStyle {
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
    });
    let upright = vec![DrawOp::Label {
        anchor: (32.0, 32.0),
        text: "ABC".into(),
        style: label_style.clone(),
        angle_rad: 0.0,
    }];
    let rotated = vec![DrawOp::Label {
        anchor: (32.0, 32.0),
        text: "ABC".into(),
        style: label_style,
        angle_rad: std::f32::consts::FRAC_PI_2,
    }];
    let (w, _, up_rgba) = decode(&render_png(canvas, &upright));
    let (_, _, rot_rgba) = decode(&render_png(canvas, &rotated));

    let painted_extents = |rgba: &[u8]| {
        let mut minx = i32::MAX;
        let mut maxx = i32::MIN;
        let mut miny = i32::MAX;
        let mut maxy = i32::MIN;
        for (i, p) in rgba.chunks_exact(4).enumerate() {
            if p[3] == 0 {
                continue;
            }
            let x = (i % w as usize) as i32;
            let y = (i / w as usize) as i32;
            if x < minx {
                minx = x;
            }
            if x > maxx {
                maxx = x;
            }
            if y < miny {
                miny = y;
            }
            if y > maxy {
                maxy = y;
            }
        }
        (maxx - minx, maxy - miny)
    };
    let (uw, uh) = painted_extents(&up_rgba);
    let (rw, rh) = painted_extents(&rot_rgba);
    assert!(uw > uh, "upright label not horizontally extended: {uw}x{uh}");
    assert!(rh > rw, "rotated label not vertically extended: {rw}x{rh}");
}

#[test]
fn measure_text_returns_font_aware_metrics() {
    let r = TinySkiaRenderer::new(Arc::new(mars_text::Fonts::with_default()));
    let style = ResolvedLabelStyle {
        font_family: "DejaVu Sans".into(),
        font_size: 12.0,
        fill: Colour::rgba(0, 0, 0, 255),
        halo: None,
        priority: 0,
        min_distance: 0.0,
        position: AnchorPosition::default(),
        offset_px: (0.0, 0.0),
        angle_deg: None,
        partials: false,
        force: false,
    };
    let m = r.measure_text("hello", &style).unwrap();
    assert!(m.advance_x.is_finite() && m.advance_x > 0.0);
    assert!(m.ascent.is_finite() && m.ascent > 0.0);
    assert!(m.descent.is_finite() && m.descent >= 0.0);
    let zero = r.measure_text("", &style).unwrap();
    assert_eq!(zero.advance_x, 0.0);
    assert!((m.ascent - zero.ascent).abs() < 1e-3);
}

#[test]
fn measure_text_unknown_font_falls_back_to_default() {
    let r = TinySkiaRenderer::new(Arc::new(mars_text::Fonts::with_default()));
    let style = ResolvedLabelStyle {
        font_family: "no-such-font-12345".into(),
        font_size: 12.0,
        fill: Colour::rgba(0, 0, 0, 255),
        halo: None,
        priority: 0,
        min_distance: 0.0,
        position: AnchorPosition::default(),
        offset_px: (0.0, 0.0),
        angle_deg: None,
        partials: false,
        force: false,
    };
    let m = r.measure_text("x", &style).unwrap();
    assert!(m.advance_x > 0.0 && m.ascent > 0.0);
}

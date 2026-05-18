#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use mars_render_port::{Canvas, Encoder, ImageFormat, Renderer};
use mars_style::Colour;

use crate::{TinySkiaEncoder, TinySkiaRenderer};

fn white() -> Colour {
    Colour {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    }
}

fn render_png(canvas: Canvas) -> Vec<u8> {
    let pm = TinySkiaRenderer::new(Arc::new(mars_text::Fonts::with_default()))
        .render(canvas, &[])
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
fn transparent_vs_opaque_background() {
    let c1 = Canvas {
        width: 4,
        height: 4,
        background: None,
    };
    let png1 = render_png(c1);
    let (_, _, rgba1) = decode(&png1);
    assert_eq!(rgba1[3], 0, "transparent bg first pixel alpha 0");

    let c2 = Canvas {
        width: 4,
        height: 4,
        background: Some(white()),
    };
    let png2 = render_png(c2);
    let (_, _, rgba2) = decode(&png2);
    assert_eq!(rgba2[3], 255, "opaque bg first pixel alpha 255");
    assert_eq!(&rgba2[0..3], &[255, 255, 255]);
}

//! png encoding.

use std::cell::RefCell;

use mars_render_port::{EncodeError, Pixmap};

use super::demultiply_into;
use crate::PngCompression;

thread_local! {
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn encode_png(pm: &Pixmap, compression: PngCompression) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::with_capacity(pm.premultiplied_rgba.len() / 2);
    {
        let mut enc = ::png::Encoder::new(&mut out, pm.width, pm.height);
        enc.set_color(::png::ColorType::Rgba);
        enc.set_depth(::png::BitDepth::Eight);
        enc.set_compression(compression.to_png());
        let mut writer = enc
            .write_header()
            .map_err(|e| EncodeError::Backend(format!("png header: {e}")))?;
        SCRATCH.with(|s| {
            let mut scratch = s.borrow_mut();
            scratch.clear();
            scratch.reserve(pm.premultiplied_rgba.len());
            demultiply_into(&pm.premultiplied_rgba, &mut scratch);
            writer
                .write_image_data(&scratch)
                .map_err(|e| EncodeError::Backend(format!("png write: {e}")))
        })?;
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use mars_render_port::{Canvas, DrawOp, Encoder, ImageFormat, Path as PortPath, Renderer, Subpath};
    use mars_style::{Colour, FillPaint, Style};

    use crate::{PngCompression, TinySkiaEncoder, TinySkiaRenderer};

    fn red() -> Colour {
        Colour::rgba(255, 0, 0, 255)
    }

    fn white() -> Colour {
        Colour::rgba(255, 255, 255, 255)
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

    fn decode(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let dec = ::png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = dec.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        buf.truncate(info.buffer_size());
        (info.width, info.height, buf)
    }

    #[test]
    fn png_compression_levels_decode_to_identical_pixels() {
        let canvas = Canvas {
            width: 32,
            height: 32,
            background: Some(white()),
        };
        let ops = vec![DrawOp::Path {
            path: square(16.0, 16.0, 8.0),
            style: Arc::new(Style {
                fill: Some(FillPaint::Solid(red())),
                ..Default::default()
            }),
        }];
        let pm = TinySkiaRenderer::new(Arc::new(mars_text::Fonts::with_default()))
            .render(canvas, &ops)
            .unwrap();
        let levels = [
            PngCompression::None,
            PngCompression::Fastest,
            PngCompression::Fast,
            PngCompression::Balanced,
            PngCompression::High,
        ];
        let decoded: Vec<_> = levels
            .iter()
            .map(|&c| {
                let enc = TinySkiaEncoder::new(85, c);
                let bytes = enc.encode(&pm, ImageFormat::Png).unwrap();
                decode(&bytes)
            })
            .collect();
        // every level must round-trip to the same pixel buffer; encoded bytes
        // legitimately differ.
        let (w0, h0, ref rgba0) = decoded[0];
        for (i, (w, h, rgba)) in decoded.iter().enumerate().skip(1) {
            assert_eq!((*w, *h), (w0, h0), "level {i} dimension mismatch");
            assert_eq!(rgba, rgba0, "level {i} pixel mismatch");
        }
    }
}

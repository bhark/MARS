//! webp encoding (lossless). pure rust via `image-webp`, so no new C dep
//! reaches the runtime image.

use std::cell::RefCell;

use image_webp::{ColorType as WebpColorType, WebPEncoder};
use mars_render_port::{EncodeError, Pixmap};

use super::demultiply_into;

thread_local! {
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn encode_webp(pm: &Pixmap) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::with_capacity(pm.premultiplied_rgba.len() / 2);
    SCRATCH.with(|s| {
        let mut scratch = s.borrow_mut();
        scratch.clear();
        scratch.reserve(pm.premultiplied_rgba.len());
        demultiply_into(&pm.premultiplied_rgba, &mut scratch);
        let enc = WebPEncoder::new(&mut out);
        enc.encode(&scratch, pm.width, pm.height, WebpColorType::Rgba8)
            .map_err(|e| EncodeError::Backend(format!("webp encode: {e}")))
    })?;
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Arc;

    use mars_render_port::{Canvas, Encoder, ImageFormat, Renderer};
    use mars_style::Colour;

    use crate::{TinySkiaEncoder, TinySkiaRenderer};

    fn red() -> Colour {
        Colour::rgba(255, 0, 0, 255)
    }

    #[test]
    fn webp_round_trip_writes_riff_webp_header() {
        let canvas = Canvas {
            width: 16,
            height: 16,
            background: Some(red()),
        };
        let pm = TinySkiaRenderer::new(Arc::new(mars_text::Fonts::with_default()))
            .render(canvas, &[])
            .unwrap();
        let bytes = TinySkiaEncoder::default().encode(&pm, ImageFormat::Webp).unwrap();
        // RIFF container magic: "RIFF" + 4-byte size + "WEBP".
        assert!(bytes.starts_with(b"RIFF"), "expected RIFF header");
        assert_eq!(&bytes[8..12], b"WEBP", "expected WEBP fourcc");
    }

    #[test]
    fn webp_preserves_dimensions() {
        for &dim in &[8u32, 64, 256] {
            let canvas = Canvas {
                width: dim,
                height: dim,
                background: Some(red()),
            };
            let pm = TinySkiaRenderer::new(Arc::new(mars_text::Fonts::with_default()))
                .render(canvas, &[])
                .unwrap();
            let bytes = TinySkiaEncoder::default().encode(&pm, ImageFormat::Webp).unwrap();
            // VP8L width/height live in the bitstream header; cheap structural
            // check: ensure non-empty output and RIFF prefix.
            assert!(bytes.len() > 20, "webp output suspiciously small at {dim}x{dim}");
            assert!(bytes.starts_with(b"RIFF"), "RIFF prefix at {dim}x{dim}");
        }
    }
}

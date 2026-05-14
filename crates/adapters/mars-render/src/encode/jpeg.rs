//! jpeg encoding.

use std::cell::RefCell;

use jpeg_encoder::{ColorType as JpegColorType, Encoder as JpegEnc};
use mars_render_port::{EncodeError, Pixmap};

thread_local! {
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// jpeg has no alpha; flatten over an opaque white background. matches the
/// conventional WMS interpretation of opaque jpeg responses.
pub(crate) fn encode_jpeg(pm: &Pixmap, quality: u8) -> Result<Vec<u8>, EncodeError> {
    let pixels = (pm.width as usize)
        .checked_mul(pm.height as usize)
        .ok_or_else(|| EncodeError::Backend(format!("dim overflow {}x{}", pm.width, pm.height)))?;
    let mut out = Vec::with_capacity(pixels / 4);
    SCRATCH.with(|s| {
        let mut scratch = s.borrow_mut();
        scratch.clear();
        scratch.reserve(pixels * 3);
        flatten_premul_over_white(&pm.premultiplied_rgba, &mut scratch);
        let enc = JpegEnc::new(&mut out, quality);
        let width = u16::try_from(pm.width)
            .map_err(|_| EncodeError::Backend(format!("jpeg width {} exceeds u16::MAX", pm.width)))?;
        let height = u16::try_from(pm.height)
            .map_err(|_| EncodeError::Backend(format!("jpeg height {} exceeds u16::MAX", pm.height)))?;
        enc.encode(&scratch, width, height, JpegColorType::Rgb)
            .map_err(|e| EncodeError::Backend(format!("jpeg encode: {e}")))
    })?;
    Ok(out)
}

/// composite premultiplied rgba over opaque white into rgb.
/// for premul (R',G',B',A): out = R' + (255 - A), since the white contribution
/// is (255 - A) * 255 / 255 = 255 - A. saturating add guards rounding drift.
fn flatten_premul_over_white(premul: &[u8], out: &mut Vec<u8>) {
    let len = premul.len() / 4 * 4;
    out.reserve(len / 4 * 3);
    for px in premul[..len].chunks_exact(4) {
        let a = px[3];
        if a == 255 {
            out.extend_from_slice(&px[..3]);
        } else {
            let inv = 255 - a;
            out.extend_from_slice(&[
                px[0].saturating_add(inv),
                px[1].saturating_add(inv),
                px[2].saturating_add(inv),
            ]);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use mars_render_port::{Canvas, Encoder, ImageFormat, Renderer};
    use mars_style::Colour;

    use crate::{TinySkiaEncoder, TinySkiaRenderer};

    fn red() -> Colour {
        Colour::rgba(255, 0, 0, 255)
    }

    /// exercises the encoder at three sizes so an mcu-boundary regression
    /// cannot hide behind the 16x16 single-mcu degenerate case: 16 = 1 mcu
    /// per side, 512 = exactly 32 mcus per side, 510 = non-multiple-of-16
    /// chroma-upsampling boundary.
    #[test]
    fn jpeg_roundtrip_decodes_to_expected_dimensions() {
        for &dim in &[16u32, 510, 512] {
            let canvas = Canvas {
                width: dim,
                height: dim,
                background: Some(red()),
            };
            let pm = TinySkiaRenderer::new(Arc::new(mars_text::Fonts::with_default()))
                .render(canvas, &[])
                .unwrap();
            let bytes = TinySkiaEncoder::default().encode(&pm, ImageFormat::Jpeg).unwrap();
            assert!(bytes.starts_with(&[0xFF, 0xD8]), "jpeg SOI marker at {dim}x{dim}");

            let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(&bytes));
            let pixels = dec.decode().unwrap();
            let info = dec.info().unwrap();
            assert_eq!((info.width, info.height), (dim as u16, dim as u16), "dim {dim}");
            // pin the encoder's output colour space; downstream decoders
            // (and any tooling that reads back pixels) assume RGB24.
            assert_eq!(
                info.pixel_format,
                jpeg_decoder::PixelFormat::RGB24,
                "expected RGB24 output at {dim}x{dim}"
            );

            // mid-image sample (not corner) so chroma-upsampling artefacts at
            // the right/bottom edge of non-multiple-of-16 sizes can't hide a
            // colour-channel swap.
            let mid = ((dim as usize / 2) * dim as usize + (dim as usize / 2)) * 3;
            let (r, g, b) = (pixels[mid], pixels[mid + 1], pixels[mid + 2]);
            assert!(
                r > 200 && g < 60 && b < 60,
                "expected red-ish at {dim}x{dim}, got ({r},{g},{b})"
            );
        }
    }

    #[test]
    fn jpeg_transparent_collapses_to_white() {
        let canvas = Canvas {
            width: 8,
            height: 8,
            background: None,
        };
        let pm = TinySkiaRenderer::new(Arc::new(mars_text::Fonts::with_default()))
            .render(canvas, &[])
            .unwrap();
        let bytes = TinySkiaEncoder::default().encode(&pm, ImageFormat::Jpeg).unwrap();
        let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(&bytes));
        let pixels = dec.decode().unwrap();
        let (r, g, b) = (pixels[0], pixels[1], pixels[2]);
        assert!(r > 240 && g > 240 && b > 240, "expected white-ish, got ({r},{g},{b})");
    }
}

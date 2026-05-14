//! Shared decoders for encoded image bytes into the port-level
//! [`DecodedImage`] (straight RGBA). Used by both the manifest-bound image
//! registry and the raster-tile render path; centralising avoids the two
//! sites diverging on subtle decode semantics.

use std::sync::Arc;

use mars_render_port::DecodedImage;

/// Decode encoded image bytes into straight RGBA, dispatching on the
/// caller-provided media type. Unknown / unsupported types surface as a
/// `Err` string the caller wraps into its local error type.
pub(crate) fn decode_to_rgba(bytes: &[u8], content_type: &str) -> Result<DecodedImage, String> {
    match content_type {
        "image/png" => decode_png_to_rgba(bytes),
        "image/jpeg" => decode_jpeg_to_rgba(bytes),
        other => Err(format!("unsupported tile content type {other:?}")),
    }
}

/// Decode PNG bytes into straight RGBA. Handles every channel layout the
/// `png` crate exposes (Rgb / Rgba / Grayscale / GrayscaleAlpha); palette
/// and 16-bit variants are not normalised here and surface as an explicit
/// error so the gap is visible.
pub(crate) fn decode_png_to_rgba(bytes: &[u8]) -> Result<DecodedImage, String> {
    let dec = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = dec.read_info().map_err(|e| format!("png header: {e}"))?;
    let buf_size = reader
        .output_buffer_size()
        .ok_or_else(|| "png buffer size unknown".to_string())?;
    let mut buf = vec![0u8; buf_size];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("png frame: {e}"))?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(buf.len() / 3 * 4);
            for px in buf.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(buf.len() * 4);
            for &g in &buf {
                out.extend_from_slice(&[g, g, g, 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(buf.len() * 2);
            for px in buf.chunks_exact(2) {
                out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
            out
        }
        other => return Err(format!("unsupported png colour type {other:?}")),
    };
    Ok(DecodedImage {
        width: info.width,
        height: info.height,
        rgba: Arc::new(rgba),
    })
}

/// Decode JPEG bytes into straight RGBA. zune-jpeg yields interleaved RGB
/// (alpha is synthesised as opaque) so XYZ tile sources serving JPEG can
/// composite alongside PNG without alpha drift.
pub(crate) fn decode_jpeg_to_rgba(bytes: &[u8]) -> Result<DecodedImage, String> {
    let mut decoder = zune_jpeg::JpegDecoder::new(zune_jpeg::zune_core::bytestream::ZCursor::new(bytes));
    decoder
        .decode_headers()
        .map_err(|e| format!("jpeg headers: {e}"))?;
    let (width, height) = decoder
        .dimensions()
        .ok_or_else(|| "jpeg dimensions unavailable".to_string())?;
    let rgb = decoder.decode().map_err(|e| format!("jpeg decode: {e}"))?;
    let expected = width.checked_mul(height).and_then(|n| n.checked_mul(3)).unwrap_or(0);
    if rgb.len() != expected {
        return Err(format!(
            "jpeg buffer length {} does not match {}x{}x3",
            rgb.len(),
            width,
            height
        ));
    }
    let mut out = Vec::with_capacity(width.saturating_mul(height).saturating_mul(4));
    for px in rgb.chunks_exact(3) {
        out.extend_from_slice(&[px[0], px[1], px[2], 255]);
    }
    let w = u32::try_from(width).map_err(|_| "jpeg width exceeds u32".to_string())?;
    let h = u32::try_from(height).map_err(|_| "jpeg height exceeds u32".to_string())?;
    Ok(DecodedImage {
        width: w,
        height: h,
        rgba: Arc::new(out),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn encode_png_rgba(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(rgba).unwrap();
        drop(writer);
        out
    }

    #[test]
    fn decode_png_roundtrips_2x2_rgba() {
        let rgba = vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 128];
        let bytes = encode_png_rgba(2, 2, &rgba);
        let decoded = decode_png_to_rgba(&bytes).unwrap();
        assert_eq!(decoded.width, 2);
        assert_eq!(decoded.height, 2);
        assert_eq!(decoded.rgba.as_slice(), rgba.as_slice());
    }

    #[test]
    fn decode_to_rgba_routes_by_content_type() {
        let rgba = vec![10, 20, 30, 255];
        let png_bytes = encode_png_rgba(1, 1, &rgba);
        let d = decode_to_rgba(&png_bytes, "image/png").unwrap();
        assert_eq!(d.rgba.as_slice(), rgba.as_slice());
        let err = decode_to_rgba(&png_bytes, "image/webp").expect_err("unsupported");
        assert!(err.contains("unsupported"));
    }
}

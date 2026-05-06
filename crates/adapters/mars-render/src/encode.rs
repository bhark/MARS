//! image encoding for png and jpeg.

use std::cell::RefCell;

use jpeg_encoder::{ColorType as JpegColorType, Encoder as JpegEnc};
use mars_render_port::{EncodeError, Pixmap};

thread_local! {
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn encode_png(pm: &Pixmap) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::with_capacity(pm.premultiplied_rgba.len() / 2);
    {
        let mut enc = png::Encoder::new(&mut out, pm.width, pm.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Balanced);
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
    for px in premul.chunks_exact(4) {
        let inv = 255u8.saturating_sub(px[3]);
        out.push(px[0].saturating_add(inv));
        out.push(px[1].saturating_add(inv));
        out.push(px[2].saturating_add(inv));
    }
}

fn demultiply_into(premul: &[u8], out: &mut Vec<u8>) {
    for px in premul.chunks_exact(4) {
        let (r, g, b, a) = (px[0], px[1], px[2], px[3]);
        if a == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
        } else if a == 255 {
            out.extend_from_slice(&[r, g, b, a]);
        } else {
            let inv = 255.0 / a as f32;
            out.push(((r as f32 * inv).round().min(255.0)) as u8);
            out.push(((g as f32 * inv).round().min(255.0)) as u8);
            out.push(((b as f32 * inv).round().min(255.0)) as u8);
            out.push(a);
        }
    }
}

//! image encoding for png and jpeg.

use std::cell::RefCell;

use jpeg_encoder::{ColorType as JpegColorType, Encoder as JpegEnc};
use mars_render_port::{EncodeError, Pixmap};

use crate::PngCompression;

thread_local! {
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn encode_png(pm: &Pixmap, compression: PngCompression) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::with_capacity(pm.premultiplied_rgba.len() / 2);
    {
        let mut enc = png::Encoder::new(&mut out, pm.width, pm.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
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

/// 1/a * 255 in 8.8 fixed-point: `INV_ALPHA[a]` × channel × 1/256 ≈ channel * 255 / a.
/// the +0.5 correction keeps the average rounding error within 0.5 LSB across the
/// whole 0..=255 range; spot-checked against the prior f32 path on a uniform grid
/// of (channel, alpha) pairs and matches within ±1 LSB for a > 0.
const INV_ALPHA: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut a = 1usize;
    while a <= 255 {
        // (255 << 8) / a, rounded
        t[a] = ((255u32 << 8) + (a as u32 / 2)) / a as u32;
        a += 1;
    }
    t
};

fn demultiply_into(premul: &[u8], out: &mut Vec<u8>) {
    let len = premul.len() / 4 * 4;
    out.reserve(len);
    let dst_start = out.len();
    // safety-equivalent prealloc: extend uninit-style via push so existing
    // unsafe-free invariant is preserved; the common a==255 path is a tight
    // 4-byte memcpy via a single extend_from_slice.
    for px in premul[..len].chunks_exact(4) {
        let a = px[3];
        match a {
            0 => out.extend_from_slice(&[0, 0, 0, 0]),
            255 => out.extend_from_slice(px),
            _ => {
                let inv = INV_ALPHA[a as usize];
                let r = ((px[0] as u32 * inv + 128) >> 8).min(255) as u8;
                let g = ((px[1] as u32 * inv + 128) >> 8).min(255) as u8;
                let b = ((px[2] as u32 * inv + 128) >> 8).min(255) as u8;
                out.extend_from_slice(&[r, g, b, a]);
            }
        }
    }
    debug_assert_eq!(out.len() - dst_start, len);
}

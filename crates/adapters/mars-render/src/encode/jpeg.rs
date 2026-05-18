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
mod tests;

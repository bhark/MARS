//! png encoding.

use std::cell::RefCell;

use mars_render_port::{EncodeError, Pixmap};

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
    // common a==255 path is a tight 4-byte memcpy via a single extend_from_slice.
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

//! image encoding. PNG only at Phase 0; JPEG deferred to Phase 1.

use std::cell::RefCell;

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

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
mod tests;

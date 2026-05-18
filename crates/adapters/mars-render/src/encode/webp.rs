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
mod tests;

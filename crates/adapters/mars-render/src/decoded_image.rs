//! Shared conversion from a port-level [`DecodedImage`] (straight RGBA) to a
//! tiny-skia [`Pixmap`] with premultiplied alpha. Used by both image-fill
//! pattern shaders and raster tile composites.

use mars_render_port::{DecodedImage, RenderError};
use tiny_skia::{Pixmap, PremultipliedColorU8};

/// Convert a straight-RGBA [`DecodedImage`] into a tiny-skia [`Pixmap`].
/// Returns [`RenderError::Backend`] when allocation fails, the rgba buffer
/// length disagrees with `width*height*4`, or premultiplication produces an
/// invalid pixel (the latter is structurally impossible but still typed).
pub(crate) fn build_premultiplied(image: &DecodedImage) -> Result<Pixmap, RenderError> {
    let mut tile = Pixmap::new(image.width, image.height)
        .ok_or_else(|| RenderError::Backend(format!("image tile alloc {}x{} failed", image.width, image.height)))?;
    let expected = (image.width as usize) * (image.height as usize) * 4;
    if image.rgba.len() != expected {
        return Err(RenderError::Backend(format!(
            "image rgba length {} does not match {}x{}",
            image.rgba.len(),
            image.width,
            image.height
        )));
    }
    let dst = tile.pixels_mut();
    for (i, src) in image.rgba.chunks_exact(4).enumerate() {
        let r = src[0];
        let g = src[1];
        let b = src[2];
        let a = src[3];
        // straight rgba -> premultiplied
        let pr = ((u16::from(r) * u16::from(a) + 127) / 255) as u8;
        let pg = ((u16::from(g) * u16::from(a) + 127) / 255) as u8;
        let pb = ((u16::from(b) * u16::from(a) + 127) / 255) as u8;
        dst[i] = PremultipliedColorU8::from_rgba(pr, pg, pb, a)
            .ok_or_else(|| RenderError::Backend("invalid premultiplied tile pixel".into()))?;
    }
    Ok(tile)
}

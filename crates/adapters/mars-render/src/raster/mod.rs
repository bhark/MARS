//! Raster-tile compositor. Decodes a tile (passed in as straight RGBA) to
//! premultiplied alpha and blits it into the destination rectangle with
//! bilinear filtering. Mirrors the image-fill pattern shader; the only
//! difference is that raster tiles paint a rectangle (`draw_pixmap`) rather
//! than fill a path with a tiled shader.

use mars_render_port::{DecodedImage, PixelRect, RenderError};
use tiny_skia::{BlendMode, FilterQuality, Pixmap, PixmapPaint, Transform};

use crate::canvas::map_blend;
use crate::decoded_image::build_premultiplied;

pub(crate) fn draw(
    pm: &mut Pixmap,
    tile: &DecodedImage,
    dst: PixelRect,
    opacity: f32,
    blend_mode: Option<mars_style::BlendMode>,
) -> Result<(), RenderError> {
    if dst.w <= 0.0 || dst.h <= 0.0 {
        return Err(RenderError::Backend(format!(
            "DrawOp::Raster dst has non-positive dimensions w={} h={}",
            dst.w, dst.h
        )));
    }
    if !dst.x.is_finite() || !dst.y.is_finite() || !dst.w.is_finite() || !dst.h.is_finite() {
        return Err(RenderError::Backend(
            "DrawOp::Raster dst carries non-finite coordinates".into(),
        ));
    }
    if tile.width == 0 || tile.height == 0 {
        return Err(RenderError::Backend("DrawOp::Raster tile is zero-sized".into()));
    }

    let tile_pm = build_premultiplied(tile)?;
    let sx = dst.w / tile.width as f32;
    let sy = dst.h / tile.height as f32;
    // pre_scale composes the scale onto the input space so points run
    // tile-space -> scale -> translate -> canvas-space.
    let transform = Transform::from_translate(dst.x, dst.y).pre_scale(sx, sy);
    let paint = PixmapPaint {
        opacity: opacity.clamp(0.0, 1.0),
        blend_mode: blend_mode.map(map_blend).unwrap_or(BlendMode::SourceOver),
        quality: FilterQuality::Bilinear,
    };
    pm.draw_pixmap(0, 0, tile_pm.as_ref(), &paint, transform, None);
    Ok(())
}

#[cfg(test)]
mod tests;

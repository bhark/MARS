//! tiled image pattern fill. Resolves `FillPaint::Image { name }` against
//! the renderer's `ImageRegistry`, builds a tiny-skia tile-pattern shader,
//! and fills the path. Unknown names surface as
//! [`RenderError::ImageNotFound`] so the runtime can distinguish "asset
//! missing" from "feature not implemented".

use mars_render_port::{ImageRegistry, RenderError};
use tiny_skia::{BlendMode, FillRule, FilterQuality, Paint, Pattern as SkPattern, Pixmap, SpreadMode, Transform};

use crate::decoded_image::build_premultiplied;

pub(crate) fn draw(
    pm: &mut Pixmap,
    path: &tiny_skia::Path,
    name: &str,
    alpha: f32,
    blend_mode: BlendMode,
    images: &dyn ImageRegistry,
) -> Result<(), RenderError> {
    let image = images
        .get(name)
        .ok_or_else(|| RenderError::ImageNotFound { name: name.into() })?;
    let tile = build_premultiplied(&image)?;
    let pattern = SkPattern::new(
        tile.as_ref(),
        SpreadMode::Repeat,
        FilterQuality::Nearest,
        alpha.clamp(0.0, 1.0),
        Transform::identity(),
    );
    let paint = Paint {
        shader: pattern,
        anti_alias: true,
        blend_mode,
        ..Default::default()
    };
    pm.fill_path(path, &paint, FillRule::EvenOdd, Transform::identity(), None);
    Ok(())
}

#[cfg(test)]
mod tests;

//! image / svg / gradient fill dispatch hub. mirrors `fill/mod.rs` but
//! for non-procedural fill paints reached through `DrawOp::Pattern`.
//!
//! procedural variants (`Solid`, `Hatch`) returning here mean the runtime
//! emitted the wrong DrawOp variant; the typed error makes the contract
//! mismatch visible at the renderer seam instead of silently rendering.

mod image;

use mars_render_port::{ImageRegistry, RenderError};
use mars_style::FillPaint;
use tiny_skia::{BlendMode, Pixmap};

use crate::path::is_fillable;
use crate::prepare::ResolvedFill;

pub(crate) fn draw(
    pm: &mut Pixmap,
    path: &tiny_skia::Path,
    fill: &ResolvedFill,
    blend_mode: BlendMode,
    images: &dyn ImageRegistry,
) -> Result<(), RenderError> {
    if !is_fillable(path) {
        return Ok(());
    }
    match &fill.paint {
        FillPaint::Image { name } => image::draw(pm, path, name, fill.alpha, blend_mode, images),
        FillPaint::Solid(_) | FillPaint::Hatch { .. } => Err(RenderError::Backend(
            "procedural fill paint emitted as DrawOp::Pattern; runtime must emit DrawOp::Path".into(),
        )),
    }
}

#[cfg(test)]
mod tests;

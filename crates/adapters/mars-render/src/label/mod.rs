//! label compositing pipeline.
//!
//! `draw` is the orchestrator: shape and rasterise the text, optionally stamp
//! the halo, then composite the fill colour. axis-aligned labels take the
//! `AxisSampler` fast path; rotated (line-label) runs go through
//! `RotatedSampler`.

pub(crate) mod compose;
pub(crate) mod follow;
mod halo;

use mars_render_port::RenderError;
use mars_style::ResolvedLabelStyle;
use mars_text::Fonts;
use tiny_skia::Pixmap;

pub(crate) fn draw(
    pm: &mut Pixmap,
    anchor: (f32, f32),
    text: &str,
    style: &ResolvedLabelStyle,
    angle_rad: f32,
    fonts: &Fonts,
) -> Result<(), RenderError> {
    let run = mars_text::measure(text, style, fonts).map_err(|e| RenderError::Backend(format!("font measure: {e}")))?;
    let mask = mars_text::rasterise(&run).map_err(|e| RenderError::Backend(format!("font rasterise: {e}")))?;
    if mask.coverage.is_empty() {
        return Ok(());
    }
    if let Some(h) = &style.halo {
        halo::stamp(pm, &mask, anchor, h, angle_rad);
    }
    let axis_aligned = angle_rad.abs() < f32::EPSILON;
    if axis_aligned {
        compose::stamp_axis(pm, &mask, anchor, style.fill, (0.0, 0.0));
    } else {
        compose::stamp_rotated(pm, &mask, anchor, style.fill, (0.0, 0.0), angle_rad);
    }
    Ok(())
}

#[cfg(test)]
mod tests;

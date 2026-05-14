//! glyph marker: shape and rasterise a single character (or grapheme
//! cluster) via mars-text, then composite the resulting mask centred on the
//! marker anchor. routes through the same `label::compose` pipeline so
//! axis-aligned and rotated stamps share one path.
//!
//! the marker contract treats `anchor` as the symbol *centre*. label compose
//! treats anchor as the baseline; the mask is offset by
//! `mask.origin + offset` from there. to centre, we set
//! `offset = -(mask.origin + half_extent)` so the mask bbox centre lands on
//! the anchor before rotation is applied.

use mars_render_port::RenderError;
use mars_style::{Colour, FillPaint, LabelStyle, Style};
use mars_text::Fonts;
use tiny_skia::Pixmap;

use crate::label::compose;

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw(
    pm: &mut Pixmap,
    anchor: (f32, f32),
    rotation_rad: f32,
    font_family: &str,
    ch: &str,
    size: f32,
    style: &Style,
    fonts: &Fonts,
) -> Result<(), RenderError> {
    if ch.is_empty() {
        return Err(RenderError::Backend("MarkerSymbol::Glyph with empty ch".into()));
    }
    let colour = effective_colour(style)?;

    let label_style = LabelStyle {
        font_family: font_family.to_string(),
        font_size: size,
        fill: colour,
        halo: None,
        priority: 0,
        min_distance: 0.0,
    };

    let run =
        mars_text::measure(ch, &label_style, fonts).map_err(|e| RenderError::Backend(format!("font measure: {e}")))?;
    let mask = mars_text::rasterise(&run).map_err(|e| RenderError::Backend(format!("font rasterise: {e}")))?;
    if mask.coverage.is_empty() {
        return Ok(());
    }

    let offset = (
        -(mask.origin_x as f32) - (mask.width as f32) / 2.0,
        -(mask.origin_y as f32) - (mask.height as f32) / 2.0,
    );

    if rotation_rad.abs() < f32::EPSILON {
        compose::stamp_axis(pm, &mask, anchor, colour, offset);
    } else {
        compose::stamp_rotated(pm, &mask, anchor, colour, offset, rotation_rad);
    }
    Ok(())
}

// fold style.opacity into the colour's alpha so the glyph honours the same
// opacity contract as the path pipeline (resolve() bakes opacity into
// ResolvedFill::alpha; we apply it directly here since glyph never goes
// through prepare).
fn effective_colour(style: &Style) -> Result<Colour, RenderError> {
    let opacity = style.opacity.unwrap_or(1.0).clamp(0.0, 1.0);
    let mut c = match &style.fill {
        Some(FillPaint::Solid(c)) => *c,
        Some(FillPaint::Hatch { .. } | FillPaint::Image { .. }) => {
            return Err(RenderError::Backend(
                "MarkerSymbol::Glyph requires a solid fill paint".into(),
            ));
        }
        None => Colour::rgba(0, 0, 0, 255),
    };
    let a = (f32::from(c.a) * opacity).round().clamp(0.0, 255.0) as u8;
    c.a = a;
    Ok(c)
}

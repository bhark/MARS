//! solid-colour polygon fill.

use mars_style::Colour;
use tiny_skia::{BlendMode, FillRule, Paint, Pixmap, Transform};

use crate::canvas::{colour_to_tsk, scaled_alpha_colour};

pub(crate) fn draw(pm: &mut Pixmap, path: &tiny_skia::Path, c: Colour, alpha: f32, blend_mode: BlendMode) {
    let colour = if alpha >= 1.0 { c } else { scaled_alpha_colour(c, alpha) };
    let mut paint = Paint::default();
    paint.set_color(colour_to_tsk(colour));
    paint.anti_alias = true;
    paint.blend_mode = blend_mode;
    pm.fill_path(path, &paint, FillRule::EvenOdd, Transform::identity(), None);
}

#[cfg(test)]
mod tests;

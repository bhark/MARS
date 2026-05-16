//! `DrawOp::Label` / `DrawOp::FollowLabel` handlers. delegate to the label
//! compositing pipeline (block-rotated vs per-glyph follow respectively).

use mars_render_port::RenderError;
use mars_style::LabelStyle;
use mars_text::Fonts;
use tiny_skia::Pixmap;

use crate::label;

pub(crate) fn draw(
    pm: &mut Pixmap,
    anchor: (f32, f32),
    text: &str,
    style: &LabelStyle,
    angle_rad: f32,
    fonts: &Fonts,
) -> Result<(), RenderError> {
    label::draw(pm, anchor, text, style, angle_rad, fonts)
}

pub(crate) fn draw_follow(
    pm: &mut Pixmap,
    polyline_px: &[(f32, f32)],
    start_arc_px: f32,
    text: &str,
    style: &LabelStyle,
    fonts: &Fonts,
) -> Result<(), RenderError> {
    label::follow::draw_follow(pm, polyline_px, start_arc_px, text, style, fonts)
}

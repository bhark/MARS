//! `DrawOp::Label` handler. delegates to the label compositing pipeline.

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

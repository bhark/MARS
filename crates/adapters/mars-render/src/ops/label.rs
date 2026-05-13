//! `DrawOp::Label` handler. delegates to the label compositing pipeline.

use mars_render_port::RenderError;
use mars_style::LabelStyle;
use mars_text::Fonts;
use tiny_skia::Pixmap;

use crate::label;
use crate::prepare::UnimplementedFeatures;

pub(crate) fn draw(
    pm: &mut Pixmap,
    anchor: (f32, f32),
    text: &str,
    style: &LabelStyle,
    angle_rad: f32,
    fonts: &Fonts,
) -> Result<UnimplementedFeatures, RenderError> {
    label::draw(pm, anchor, text, style, angle_rad, fonts).map(|()| UnimplementedFeatures::default())
}

//! `DrawOp` dispatch hub. exhaustive match on the port-level enum so that
//! adding a variant in `mars-render-port` breaks the build here, forcing the
//! implementation rather than silently falling through.

mod label;
mod path;

use mars_render_port::{DrawOp, RenderError};
use mars_text::Fonts;
use tiny_skia::Pixmap;

pub(crate) fn dispatch(pm: &mut Pixmap, op: &DrawOp, fonts: &Fonts) -> Result<(), RenderError> {
    match op {
        DrawOp::Path { path, style } => {
            path::draw(pm, path, style);
            Ok(())
        }
        DrawOp::Label {
            anchor,
            text,
            style,
            angle_rad,
        } => label::draw(pm, *anchor, text, style, *angle_rad, fonts),
    }
}

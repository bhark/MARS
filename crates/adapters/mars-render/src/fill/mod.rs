//! polygon fill pipeline. dispatch hub on `FillPaint` variant.
//!
//! adding a fill variant is mechanical: one new file, one mod line, one
//! match arm. unsupported variants surface as typed `NotImplemented`.

mod hatch;
mod solid;

use mars_render_port::RenderError;
use mars_style::FillPaint;
use tiny_skia::Pixmap;

use crate::path::is_fillable;
use crate::prepare::ResolvedFill;

pub(crate) fn draw(pm: &mut Pixmap, path: &tiny_skia::Path, fill: &ResolvedFill) -> Result<(), RenderError> {
    if !is_fillable(path) {
        return Ok(());
    }
    match fill.paint {
        FillPaint::Solid(c) => {
            solid::draw(pm, path, c, fill.alpha);
            Ok(())
        }
        FillPaint::Hatch {
            spacing,
            angle_deg,
            line_width,
            colour,
        } => {
            hatch::draw(pm, path, spacing, angle_deg, line_width, colour, fill.alpha);
            Ok(())
        }
        _ => Err(RenderError::NotImplemented {
            what: "FillPaint variant",
        }),
    }
}

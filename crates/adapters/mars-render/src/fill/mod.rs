//! polygon fill pipeline. dispatch hub on `FillPaint` variant.
//!
//! adding a fill variant is mechanical: one new file, one mod line, one
//! match arm. unsupported variants log once at debug rather than panicking.

mod hatch;
mod solid;

use mars_style::FillPaint;
use tiny_skia::Pixmap;

use crate::path::is_fillable;
use crate::prepare::ResolvedFill;

pub(crate) fn draw(pm: &mut Pixmap, path: &tiny_skia::Path, fill: &ResolvedFill) {
    if !is_fillable(path) {
        return;
    }
    match fill.paint {
        FillPaint::Solid(c) => solid::draw(pm, path, c, fill.alpha),
        FillPaint::Hatch {
            spacing,
            angle_deg,
            line_width,
            colour,
        } => hatch::draw(pm, path, spacing, angle_deg, line_width, colour, fill.alpha),
        other => {
            tracing::debug!(target: "mars_render", "unsupported FillPaint variant: {:?}", other);
        }
    }
}

//! polyline stroke pipeline.
//!
//! takes a `ResolvedStroke` (already opacity-folded and sub-pixel-clamped)
//! and emits a single tiny-skia stroke pass. supports parallel-offset via
//! `path_offset` when `offset_px != 0`. when `stroke_gap` is set, the
//! `gap` submodule stamps the parent style's marker along the line.

pub(crate) mod dash;
pub(crate) mod gap;

use mars_render_port::Path as PortPath;
use tiny_skia::{BlendMode, Paint, Pixmap, Stroke, Transform};

use crate::canvas::scaled_alpha;
use crate::path::build_path;
use crate::path_offset::offset_polyline;
use crate::prepare::ResolvedStroke;

pub(crate) fn draw(
    pm: &mut Pixmap,
    port_path: &PortPath,
    tsk_path: &tiny_skia::Path,
    stroke: &ResolvedStroke,
    blend_mode: BlendMode,
) {
    let mut paint = Paint::default();
    paint.set_color(scaled_alpha(stroke.colour, stroke.alpha));
    paint.anti_alias = true;
    paint.blend_mode = blend_mode;

    let offset_path = if stroke.offset_px != 0.0 {
        offset_polyline(port_path, stroke.offset_px).and_then(|p| build_path(&p))
    } else {
        None
    };
    let tsk_stroke = Stroke {
        width: stroke.width,
        line_cap: stroke.cap,
        line_join: stroke.join,
        dash: stroke.dash.clone(),
        ..Stroke::default()
    };
    let path = offset_path.as_ref().unwrap_or(tsk_path);
    pm.stroke_path(path, &paint, &tsk_stroke, Transform::identity(), None);
}

#[cfg(test)]
mod tests;

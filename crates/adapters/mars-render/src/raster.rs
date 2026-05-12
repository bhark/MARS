//! tiny-skia rasterisation helpers.

use mars_render_port::Path as PortPath;
use mars_style::Style;
use tiny_skia::Pixmap;

use crate::fill;
use crate::path::build_path;
use crate::prepare;
use crate::stroke;

/// draw a single styled path. uses even-odd fill rule (matches mapserver/qgis
/// expectations for self-intersecting symbol geometry; non-zero would change
/// the visual outcome of holes-as-CCW-rings produced upstream).
pub(crate) fn draw_path(pm: &mut Pixmap, path: &PortPath, style: &Style) {
    let Some(tsk_path) = build_path(path) else {
        return;
    };
    let resolved = prepare::resolve(style);

    if let Some(fill_resolved) = &resolved.fill {
        fill::draw(pm, &tsk_path, fill_resolved);
    }

    if matches!(style.marker, Some(mars_style::MarkerSymbol::Glyph { .. })) {
        tracing::debug!("Style::marker glyph rendering is not yet implemented in the renderer");
    }

    if let Some(stroke_resolved) = &resolved.stroke {
        stroke::draw(pm, path, &tsk_path, stroke_resolved);
    }
}



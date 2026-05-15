//! `DrawOp::Path` handler. assembles the fill + stroke pipeline using
//! `prepare`, `fill`, and `stroke`. uses even-odd fill rule (matches
//! mapserver/qgis expectations for self-intersecting symbol geometry;
//! non-zero would change the visual outcome of holes-as-CCW-rings produced
//! upstream).

use mars_render_port::{Path as PortPath, RenderError};
use mars_style::Style;
use tiny_skia::{Mask, Pixmap};

use crate::fill;
use crate::path::build_path;
use crate::prepare::{self, UnimplementedFeatures};
use crate::stroke;

pub(crate) fn draw(
    pm: &mut Pixmap,
    path: &PortPath,
    style: &Style,
    hatch_mask: &mut Option<Mask>,
) -> Result<UnimplementedFeatures, RenderError> {
    let Some(tsk_path) = build_path(path) else {
        return Ok(UnimplementedFeatures::default());
    };
    let resolved = prepare::resolve(style);

    if let Some(fill_resolved) = &resolved.fill {
        fill::draw(pm, &tsk_path, fill_resolved, hatch_mask)?;
    }

    if let Some(stroke_resolved) = resolved.stroke {
        stroke::draw(pm, path, &tsk_path, stroke_resolved);
    }

    Ok(resolved.unimplemented)
}

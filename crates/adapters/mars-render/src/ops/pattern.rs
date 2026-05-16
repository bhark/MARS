//! `DrawOp::Pattern` handler. mirrors `ops/path.rs` but routes through
//! `pattern::draw` rather than `fill::draw`. patterns are fill-only ops
//! today; stroke fields are deferred until a concrete stroked-pattern
//! variant appears.

use mars_render_port::{ImageRegistry, Path as PortPath, RenderError};
use mars_style::Style;
use tiny_skia::Pixmap;

use crate::path::build_path;
use crate::pattern;
use crate::prepare;

pub(crate) fn draw(
    pm: &mut Pixmap,
    path: &PortPath,
    style: &Style,
    images: &dyn ImageRegistry,
) -> Result<(), RenderError> {
    let Some(tsk_path) = build_path(path) else {
        return Ok(());
    };
    let resolved = prepare::resolve(style);
    if let Some(fill_resolved) = &resolved.fill {
        pattern::draw(pm, &tsk_path, fill_resolved, images)?;
    }
    Ok(())
}

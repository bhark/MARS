//! glyph markers paint via the label pipeline (mars-render::ops::label); we
//! emit a single-anchor subpath so the renderer can stamp at the point.

use mars_render_port::{Path, Subpath};

pub(super) fn path((cx, cy): (f32, f32)) -> Path {
    Path {
        subpaths: vec![Subpath {
            points: vec![(cx, cy)],
            closed: false,
        }],
    }
}

//! point-marker dispatch hub. mirrors the variant-per-file shape of
//! `fill/` and `stroke/`: each `MarkerShape` variant lives in a sibling
//! module and is reached through a single exhaustive match. adding a
//! variant in `mars-style` breaks the build here, forcing the conversation
//! about whether the new marker is wired or staged.
//!
//! A `None` marker on a `Symbol` op is a runtime contract slip; rendering
//! no-ops rather than aborting the batch, consistent with how empty paths
//! and zero-width strokes are tolerated elsewhere in the pipeline.

mod circle;
mod cross;
mod glyph;
mod pin;
mod square;
mod triangle;
mod vector_shape;
mod x;

use mars_render_port::{Path as PortPath, RenderError};
use mars_style::{MarkerShape, ResolvedStyle};
use mars_text::Fonts;
use tiny_skia::Pixmap;

pub(crate) fn dispatch(
    pm: &mut Pixmap,
    anchor: (f32, f32),
    rotation_rad: f32,
    style: &ResolvedStyle,
    fonts: &Fonts,
) -> Result<(), RenderError> {
    let Some(marker) = &style.marker else {
        return Ok(());
    };
    let size = marker.size;
    match &marker.shape {
        MarkerShape::Glyph { font_family, ch } => {
            glyph::draw(pm, anchor, rotation_rad, font_family, ch, size, style, fonts)
        }
        MarkerShape::Circle => render(pm, circle::build_path(size), anchor, rotation_rad, style, fonts),
        MarkerShape::Square => render(pm, square::build_path(size), anchor, rotation_rad, style, fonts),
        MarkerShape::Triangle => render(pm, triangle::build_path(size), anchor, rotation_rad, style, fonts),
        MarkerShape::Cross => render(pm, cross::build_path(size), anchor, rotation_rad, style, fonts),
        MarkerShape::X => render(pm, x::build_path(size), anchor, rotation_rad, style, fonts),
        MarkerShape::Pin => render(pm, pin::build_path(size), anchor, rotation_rad, style, fonts),
        MarkerShape::VectorShape {
            points,
            anchor: local_anchor,
            filled,
        } => {
            let path = vector_shape::build_path(points, *local_anchor, *filled, size);
            if *filled {
                render(pm, path, anchor, rotation_rad, style, fonts)
            } else {
                // open polyline: clear fill so the polygon pipeline is bypassed.
                // a fill paint on an open path would be auto-closed by
                // tiny-skia, which is the wrong semantics.
                let mut s = style.clone();
                s.fill = None;
                render(pm, path, anchor, rotation_rad, &s, fonts)
            }
        }
    }
}

// rotate each subpath point around the local origin by `rotation_rad`,
// then translate by `anchor`, and delegate to the path pipeline so fill
// and stroke flow through the same prepare / resolve / draw chain that
// DrawOp::Path uses.
fn render(
    pm: &mut Pixmap,
    mut local: PortPath,
    anchor: (f32, f32),
    rotation_rad: f32,
    style: &ResolvedStyle,
    fonts: &Fonts,
) -> Result<(), RenderError> {
    let (sin_r, cos_r) = rotation_rad.sin_cos();
    for sub in &mut local.subpaths {
        for p in &mut sub.points {
            let (x, y) = *p;
            *p = (anchor.0 + cos_r * x - sin_r * y, anchor.1 + sin_r * x + cos_r * y);
        }
    }
    crate::ops::path::draw(pm, &local, style, fonts)
}

#[cfg(test)]
mod tests;

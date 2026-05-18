//! procedural parallel-line hatch fill.
//!
//! builds a polygon clip mask from the path, then stamps strokes oriented at
//! `angle_deg` (0 = horizontal, 90 = vertical) at `spacing` intervals across
//! the path's bbox.
//!
//! degenerate inputs (non-finite or non-positive numerics) silently produce no
//! fill - config-load validation rejects these before they reach here.
//!
//! perf: per-polygon cost is one full-canvas Mask allocation + scan-line
//! rasterisation of the polygon into the mask + stroke-path along
//! `bbox_extent / spacing` lines. on 1024x1024 canvases this measures ~6-7x
//! slower than `FillPaint::Solid` (benches/hatch.rs). a future optimisation:
//! pre-render one period of the hatch into a small tileable pixmap and stamp
//! it under the mask, trading the per-polygon stroke ops for a single textured
//! fill. landed only if hatch turns up in a hot cadastral tile path; the
//! current cost is acceptable for beta.

use mars_style::Colour;
use tiny_skia::{BlendMode, FillRule, LineCap, LineJoin, Mask, Paint, PathBuilder, Pixmap, Stroke, Transform};

use crate::canvas::{colour_to_tsk, scaled_alpha_colour};

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw(
    pm: &mut Pixmap,
    path: &tiny_skia::Path,
    spacing: f32,
    angle_deg: f32,
    line_width: f32,
    colour: Colour,
    alpha: f32,
    blend_mode: BlendMode,
) {
    if !(spacing.is_finite() && spacing > 0.0 && line_width.is_finite() && line_width > 0.0 && angle_deg.is_finite()) {
        return;
    }

    let Some(mut mask) = Mask::new(pm.width(), pm.height()) else {
        return;
    };
    mask.fill_path(path, FillRule::EvenOdd, true, Transform::identity());

    // strokes are emitted in the path's local frame and oriented by
    // (cos, sin) of the requested angle. the bbox of a path rotated by
    // -angle determines how many parallel strokes we need and their span.
    let theta = angle_deg.to_radians();
    let (sin_t, cos_t) = theta.sin_cos();
    let b = path.bounds();
    let corners = [
        (b.left(), b.top()),
        (b.right(), b.top()),
        (b.right(), b.bottom()),
        (b.left(), b.bottom()),
    ];
    // project corners onto the hatch-normal axis (perpendicular to stroke
    // direction). min/max give the range of perpendicular offsets we must
    // span; project onto the parallel axis to size the stroke length.
    let (nx, ny) = (-sin_t, cos_t); // normal axis (unit)
    let (tx, ty) = (cos_t, sin_t); // tangent axis (unit)
    let mut nmin = f32::INFINITY;
    let mut nmax = f32::NEG_INFINITY;
    let mut tmin = f32::INFINITY;
    let mut tmax = f32::NEG_INFINITY;
    for (cx, cy) in corners {
        let n = cx * nx + cy * ny;
        let t = cx * tx + cy * ty;
        if n < nmin {
            nmin = n;
        }
        if n > nmax {
            nmax = n;
        }
        if t < tmin {
            tmin = t;
        }
        if t > tmax {
            tmax = t;
        }
    }
    // pad by half a line-width so strokes at the edge are not clipped at
    // their bbox boundary.
    let pad = (line_width * 0.5).max(1.0);
    tmin -= pad;
    tmax += pad;

    let mut pb = PathBuilder::new();
    let mut n = nmin;
    let max_strokes = ((nmax - nmin) / spacing).ceil() as i32 + 2;
    let mut steps = 0;
    while n <= nmax && steps < max_strokes.max(1) {
        let (ax, ay) = (tmin * tx + n * nx, tmin * ty + n * ny);
        let (bx, by) = (tmax * tx + n * nx, tmax * ty + n * ny);
        pb.move_to(ax, ay);
        pb.line_to(bx, by);
        n += spacing;
        steps += 1;
    }
    let Some(stroke_path) = pb.finish() else {
        return;
    };

    let line_colour = if alpha >= 1.0 {
        colour
    } else {
        scaled_alpha_colour(colour, alpha)
    };
    let mut paint = Paint::default();
    paint.set_color(colour_to_tsk(line_colour));
    paint.anti_alias = true;
    paint.blend_mode = blend_mode;
    let stroke = Stroke {
        width: line_width,
        line_cap: LineCap::Butt,
        line_join: LineJoin::Miter,
        ..Stroke::default()
    };
    pm.stroke_path(&stroke_path, &paint, &stroke, Transform::identity(), Some(&mask));
}

#[cfg(test)]
mod tests;

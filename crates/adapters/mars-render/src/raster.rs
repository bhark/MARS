//! tiny-skia rasterisation helpers.

use mars_render_port::{Path as PortPath, RenderError};
use mars_style::{Colour, FillPaint, LabelStyle, LineCap as SLineCap, LineJoin as SLineJoin, Style};
use mars_text::{Fonts, GlyphMask};
use tiny_skia::{Color, FillRule, LineCap, LineJoin, Mask, Paint, PathBuilder, Pixmap, Stroke, StrokeDash, Transform};

/// build a tiny-skia path from port subpaths. closed subpaths are finished
/// with `close()`, open ones are left open.
/// returns None if no subpath has at least 2 points (tiny-skia rejects empty paths).
pub(crate) fn build_path(path: &PortPath) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    let mut any = false;
    for sub in &path.subpaths {
        if sub.points.len() < 2 {
            continue;
        }
        let (x0, y0) = sub.points[0];
        pb.move_to(x0, y0);
        for &(x, y) in &sub.points[1..] {
            pb.line_to(x, y);
        }
        if sub.closed {
            pb.close();
        }
        any = true;
    }
    if !any {
        return None;
    }
    pb.finish()
}

pub(crate) fn colour_to_tsk(c: Colour) -> Color {
    Color::from_rgba8(c.r, c.g, c.b, c.a)
}

/// returns `c` with alpha multiplied by `scale` (clamped to [0,1]). used to
/// emulate AGG sub-pixel stroke widths: a width of 0.15 renders as a 1px
/// stroke at 15% alpha rather than a full-intensity 1px line.
fn scaled_alpha(c: Colour, scale: f32) -> Color {
    let s = scale.clamp(0.0, 1.0);
    let a = ((c.a as f32) * s).round().clamp(0.0, 255.0) as u8;
    Color::from_rgba8(c.r, c.g, c.b, a)
}

/// returns `c` with alpha multiplied by `scale` (clamped to [0,1]). same idea
/// as `scaled_alpha` but yields a `Colour` so callers can re-thread the
/// result through `FillPaint::Hatch::colour` or further-scaled paths.
fn scaled_alpha_colour(c: Colour, scale: f32) -> Colour {
    let s = scale.clamp(0.0, 1.0);
    let a = ((c.a as f32) * s).round().clamp(0.0, 255.0) as u8;
    Colour { r: c.r, g: c.g, b: c.b, a }
}

/// true iff the path's AABB has non-zero extent on both axes. tiny-skia's
/// `fill_path` rejects degenerate-bbox paths (collapsed to a point or a
/// horizontal/vertical line) with a `log::warn`; gating here suppresses that
/// noise for the common case of subpixel polygons after world->pixel
/// projection. threshold mirrors tiny-skia's `SCALAR_NEARLY_ZERO` (1/4096).
pub(crate) fn is_fillable(path: &tiny_skia::Path) -> bool {
    const NEARLY_ZERO: f32 = 1.0 / 4096.0;
    let b = path.bounds();
    b.width() > NEARLY_ZERO && b.height() > NEARLY_ZERO
}

fn map_cap(c: SLineCap) -> LineCap {
    match c {
        SLineCap::Butt => LineCap::Butt,
        SLineCap::Round => LineCap::Round,
        SLineCap::Square => LineCap::Square,
    }
}

fn map_join(j: SLineJoin) -> LineJoin {
    match j {
        SLineJoin::Miter => LineJoin::Miter,
        SLineJoin::Round => LineJoin::Round,
        SLineJoin::Bevel => LineJoin::Bevel,
    }
}

/// fill the pixmap with a solid colour (used for canvas background).
pub(crate) fn fill_background(pm: &mut Pixmap, c: Colour) {
    pm.fill(colour_to_tsk(c));
}

/// dispatch on the `FillPaint` variant. Solid paints with the colour; Hatch
/// rasterises a clip mask from the polygon and stamps parallel-line strokes
/// over the path's bbox at the configured angle/spacing/line-width.
fn draw_fill(pm: &mut Pixmap, path: &tiny_skia::Path, fill: FillPaint) {
    match fill {
        FillPaint::Solid(c) => {
            let mut paint = Paint::default();
            paint.set_color(colour_to_tsk(c));
            paint.anti_alias = true;
            pm.fill_path(path, &paint, FillRule::EvenOdd, Transform::identity(), None);
        }
        FillPaint::Hatch {
            spacing,
            angle_deg,
            line_width,
            colour,
        } => draw_hatch_fill(pm, path, spacing, angle_deg, line_width, colour),
        // future FillPaint variants are forward-compatible no-ops.
        _ => {}
    }
}

/// procedural parallel-line hatch fill. builds a polygon clip mask from the
/// path, then stamps strokes oriented at `angle_deg` (0 = horizontal,
/// 90 = vertical) at `spacing` intervals across the path's bbox.
///
/// degenerate inputs (non-finite or non-positive numerics) silently produce
/// no fill - config-load validation rejects these before they reach here.
///
/// perf: per-polygon cost is one full-canvas Mask allocation + scan-line
/// rasterisation of the polygon into the mask + stroke-path along
/// `bbox_extent / spacing` lines. on 1024x1024 canvases this measures
/// ~6-7x slower than `FillPaint::Solid` (benches/hatch.rs). a future
/// optimisation: pre-render one period of the hatch into a small tileable
/// pixmap and stamp it under the mask, trading the per-polygon stroke ops
/// for a single textured fill. landed only if hatch turns up in a hot
/// cadastral tile path; the current cost is acceptable for beta.
fn draw_hatch_fill(pm: &mut Pixmap, path: &tiny_skia::Path, spacing: f32, angle_deg: f32, line_width: f32, colour: Colour) {
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
    // first stroke at nmin, step by spacing until nmax.
    let mut n = nmin;
    // a near-zero spacing is filtered above; loop bound is also defensive.
    let max_strokes = ((nmax - nmin) / spacing).ceil() as i32 + 2;
    let mut steps = 0;
    while n <= nmax && steps < max_strokes.max(1) {
        // line of constant n (normal coord), parameterised by t.
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

    let mut paint = Paint::default();
    paint.set_color(colour_to_tsk(colour));
    paint.anti_alias = true;
    let stroke = Stroke {
        width: line_width,
        line_cap: LineCap::Butt,
        line_join: LineJoin::Miter,
        ..Stroke::default()
    };
    pm.stroke_path(&stroke_path, &paint, &stroke, Transform::identity(), Some(&mask));
}

/// draw a single styled path. uses even-odd fill rule (matches mapserver/qgis
/// expectations for self-intersecting symbol geometry; non-zero would change
/// the visual outcome of holes-as-CCW-rings produced upstream).
pub(crate) fn draw_path(pm: &mut Pixmap, path: &PortPath, style: &Style) {
    let Some(tsk_path) = build_path(path) else {
        return;
    };
    // style-wide opacity multiplies into fill/stroke alpha; missing or
    // out-of-range values fall through as opaque.
    let opacity = style.opacity.unwrap_or(1.0).clamp(0.0, 1.0);

    if let Some(fill) = style.fill
        && is_fillable(&tsk_path)
    {
        draw_fill(pm, &tsk_path, fill_with_opacity(fill, opacity));
    }

    if let Some(stroke_col) = style.stroke {
        // tiny-skia silently drops strokes thinner than ~0.75 px. AGG-based
        // renderers (MapServer) emulate sub-pixel widths by drawing a 1px
        // stroke at proportionally reduced alpha; mirror that here so a
        // WIDTH 0.15 outline stays soft instead of going full intensity.
        let requested = style.stroke_width.unwrap_or(1.0);
        if requested > 0.0 {
            let (width, alpha_scale) = if requested < 1.0 {
                (1.0, requested)
            } else {
                (requested, 1.0)
            };
            let mut paint = Paint::default();
            paint.set_color(scaled_alpha(stroke_col, alpha_scale * opacity));
            paint.anti_alias = true;

            let mut stroke = Stroke {
                width,
                line_cap: style.stroke_linecap.map(map_cap).unwrap_or(LineCap::Butt),
                line_join: style.stroke_linejoin.map(map_join).unwrap_or(LineJoin::Miter),
                ..Stroke::default()
            };
            if let Some(dashes) = style.stroke_dasharray.as_ref()
                && !dashes.is_empty()
            {
                stroke.dash = StrokeDash::new(dashes.clone(), 0.0);
                if stroke.dash.is_none() {
                    tracing::warn!(dashes = ?dashes, "invalid stroke dash array: odd length, rendering solid");
                }
            }
            pm.stroke_path(&tsk_path, &paint, &stroke, Transform::identity(), None);
        }
    }
}

/// apply `opacity` to a `FillPaint`. `Solid` re-wraps the alpha-scaled colour;
/// `Hatch` rescales its line colour. forward-compatible no-op for future
/// variants.
fn fill_with_opacity(fill: FillPaint, opacity: f32) -> FillPaint {
    if opacity >= 1.0 {
        return fill;
    }
    match fill {
        FillPaint::Solid(c) => FillPaint::Solid(scaled_alpha_colour(c, opacity)),
        FillPaint::Hatch {
            spacing,
            angle_deg,
            line_width,
            colour,
        } => FillPaint::Hatch {
            spacing,
            angle_deg,
            line_width,
            colour: scaled_alpha_colour(colour, opacity),
        },
        other => other,
    }
}

/// shape `text`, rasterise it once into an alpha mask, then composite the
/// mask into `pm` at `anchor` (baseline). when `style.halo` is set the mask
/// is stamped first in eight cardinal directions in the halo colour, then
/// the fill colour is laid on top. `angle_rad` rotates the entire stamp
/// counter-clockwise around `anchor`; halo offsets rotate with the mask so
/// the halo stays tangent-aligned on line labels.
pub(crate) fn draw_label(
    pm: &mut Pixmap,
    anchor: (f32, f32),
    text: &str,
    style: &LabelStyle,
    angle_rad: f32,
    fonts: &Fonts,
) -> Result<(), RenderError> {
    let run = mars_text::measure(text, style, fonts).map_err(|e| RenderError::Backend(format!("font measure: {e}")))?;
    let mask = mars_text::rasterise(&run).map_err(|e| RenderError::Backend(format!("font rasterise: {e}")))?;
    if mask.coverage.is_empty() {
        return Ok(());
    }
    // fast path: axis-aligned labels keep the existing memcpy-style row
    // walker. only line labels go through the rotated sampler.
    let axis_aligned = angle_rad.abs() < f32::EPSILON;

    if let Some(halo) = &style.halo {
        let radius = halo.width.max(0.0).round() as i32;
        if radius > 0 {
            // 8-direction offset stamp at unit step. wider halos repeat the
            // stamp at integer offsets up to `radius`. simple but cheap; the
            // perceptual budget on labelled goldens absorbs the AA jitter.
            for dx in -radius..=radius {
                for dy in -radius..=radius {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    if dx * dx + dy * dy > radius * radius {
                        continue;
                    }
                    if axis_aligned {
                        composite_mask(pm, &mask, anchor, halo.colour, (dx as f32, dy as f32));
                    } else {
                        composite_mask_rotated(pm, &mask, anchor, halo.colour, (dx as f32, dy as f32), angle_rad);
                    }
                }
            }
        }
    }

    if axis_aligned {
        composite_mask(pm, &mask, anchor, style.fill, (0.0, 0.0));
    } else {
        composite_mask_rotated(pm, &mask, anchor, style.fill, (0.0, 0.0), angle_rad);
    }
    Ok(())
}

/// `(x * y + 127) / 255` approximated as `(x*y + 0x80 + ((x*y) >> 8)) >> 8`,
/// the standard integer-/255 trick. error <= 1 LSB across the whole 0..=255
/// range; well inside font AA tolerance.
#[inline]
fn div255(v: u32) -> u32 {
    (v + 0x80 + (v >> 8)) >> 8
}

/// composite `mask` onto `pm` with a rotation by `angle_rad` around `anchor`.
/// `offset` is applied in the mask's local (pre-rotation) frame so halo
/// stamps rotate with the glyph rather than smearing outwards in canvas
/// space. nearest-neighbour sampling; aliasing is acceptable at the small
/// font sizes that drive line labels.
fn composite_mask_rotated(
    pm: &mut Pixmap,
    mask: &GlyphMask,
    anchor: (f32, f32),
    colour: Colour,
    offset: (f32, f32),
    angle_rad: f32,
) {
    if mask.width == 0 || mask.height == 0 {
        return;
    }
    let (sin_a, cos_a) = angle_rad.sin_cos();
    let pm_w = pm.width() as i32;
    let pm_h = pm.height() as i32;
    let mw = mask.width as f32;
    let mh = mask.height as f32;
    let ox = mask.origin_x as f32 + offset.0;
    let oy = mask.origin_y as f32 + offset.1;

    // forward-rotate the four mask corners (in canvas space) to bound the
    // dst rect we have to scan; round outwards by 1 px to absorb sampling
    // round-off.
    let corners = [(ox, oy), (ox + mw, oy), (ox, oy + mh), (ox + mw, oy + mh)];
    let mut minx = f32::INFINITY;
    let mut miny = f32::INFINITY;
    let mut maxx = f32::NEG_INFINITY;
    let mut maxy = f32::NEG_INFINITY;
    for &(lx, ly) in &corners {
        let rx = anchor.0 + cos_a * lx - sin_a * ly;
        let ry = anchor.1 + sin_a * lx + cos_a * ly;
        if rx < minx {
            minx = rx;
        }
        if ry < miny {
            miny = ry;
        }
        if rx > maxx {
            maxx = rx;
        }
        if ry > maxy {
            maxy = ry;
        }
    }
    let dst_x_lo = minx.floor() as i32 - 1;
    let dst_x_hi = maxx.ceil() as i32 + 1;
    let dst_y_lo = miny.floor() as i32 - 1;
    let dst_y_hi = maxy.ceil() as i32 + 1;

    let dst_x_lo = dst_x_lo.max(0);
    let dst_x_hi = dst_x_hi.min(pm_w);
    let dst_y_lo = dst_y_lo.max(0);
    let dst_y_hi = dst_y_hi.min(pm_h);
    if dst_x_lo >= dst_x_hi || dst_y_lo >= dst_y_hi {
        return;
    }

    let data = pm.data_mut();
    let sr = u32::from(colour.r);
    let sg = u32::from(colour.g);
    let sb = u32::from(colour.b);
    let sa = u32::from(colour.a);
    let mask_w = mask.width as usize;
    let mw_i = mask.width as i32;
    let mh_i = mask.height as i32;

    for dy in dst_y_lo..dst_y_hi {
        for dx in dst_x_lo..dst_x_hi {
            // inverse rotation around anchor, then back into mask-local
            // coords by subtracting the (post-offset) origin.
            let rx = dx as f32 - anchor.0;
            let ry = dy as f32 - anchor.1;
            let lx = cos_a * rx + sin_a * ry;
            let ly = -sin_a * rx + cos_a * ry;
            let mx = (lx - ox).floor() as i32;
            let my = (ly - oy).floor() as i32;
            if mx < 0 || my < 0 || mx >= mw_i || my >= mh_i {
                continue;
            }
            let cov = mask.coverage[my as usize * mask_w + mx as usize];
            if cov == 0 {
                continue;
            }
            let a_src = div255(sa * u32::from(cov));
            if a_src == 0 {
                continue;
            }
            let idx = (dy as usize * pm_w as usize + dx as usize) * 4;
            let pr = div255(sr * a_src) as u8;
            let pg = div255(sg * a_src) as u8;
            let pb = div255(sb * a_src) as u8;
            let inv = 255 - a_src;
            data[idx] = pr.saturating_add(div255(u32::from(data[idx]) * inv) as u8);
            data[idx + 1] = pg.saturating_add(div255(u32::from(data[idx + 1]) * inv) as u8);
            data[idx + 2] = pb.saturating_add(div255(u32::from(data[idx + 2]) * inv) as u8);
            data[idx + 3] = (a_src as u8).saturating_add(div255(u32::from(data[idx + 3]) * inv) as u8);
        }
    }
}

fn composite_mask(pm: &mut Pixmap, mask: &GlyphMask, anchor: (f32, f32), colour: Colour, offset: (f32, f32)) {
    if mask.width == 0 || mask.height == 0 {
        return;
    }
    let pm_w = pm.width() as i32;
    let pm_h = pm.height() as i32;
    let dst_x0 = (anchor.0 + mask.origin_x as f32 + offset.0).round() as i32;
    let dst_y0 = (anchor.1 + mask.origin_y as f32 + offset.1).round() as i32;
    let mw = mask.width as i32;
    let mh = mask.height as i32;

    // clip the dst rect once instead of branching per-pixel.
    let mx_lo = (-dst_x0).max(0).min(mw);
    let mx_hi = (pm_w - dst_x0).max(0).min(mw);
    let my_lo = (-dst_y0).max(0).min(mh);
    let my_hi = (pm_h - dst_y0).max(0).min(mh);
    if mx_lo >= mx_hi || my_lo >= my_hi {
        return;
    }

    let data = pm.data_mut();
    let sr = u32::from(colour.r);
    let sg = u32::from(colour.g);
    let sb = u32::from(colour.b);
    let sa = u32::from(colour.a);
    let mask_w = mask.width as usize;

    for my in my_lo..my_hi {
        let dy = (dst_y0 + my) as usize;
        let row_dst = dy * pm_w as usize * 4;
        let row_mask = my as usize * mask_w;
        for mx in mx_lo..mx_hi {
            let cov = mask.coverage[row_mask + mx as usize];
            if cov == 0 {
                continue;
            }
            let a_src = div255(sa * u32::from(cov));
            if a_src == 0 {
                continue;
            }
            let idx = row_dst + (dst_x0 + mx) as usize * 4;
            let pr = div255(sr * a_src) as u8;
            let pg = div255(sg * a_src) as u8;
            let pb = div255(sb * a_src) as u8;
            let inv = 255 - a_src;
            data[idx] = pr.saturating_add(div255(u32::from(data[idx]) * inv) as u8);
            data[idx + 1] = pg.saturating_add(div255(u32::from(data[idx + 1]) * inv) as u8);
            data[idx + 2] = pb.saturating_add(div255(u32::from(data[idx + 2]) * inv) as u8);
            data[idx + 3] = (a_src as u8).saturating_add(div255(u32::from(data[idx + 3]) * inv) as u8);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_render_port::Subpath;

    fn build(points: Vec<(f32, f32)>, closed: bool) -> Option<tiny_skia::Path> {
        build_path(&PortPath {
            subpaths: vec![Subpath { points, closed }],
        })
    }

    #[test]
    fn build_path_drops_subpath_with_single_point() {
        assert!(build(vec![(1.0, 2.0)], false).is_none());
    }

    #[test]
    fn is_fillable_rejects_horizontal_line() {
        let p = build(vec![(0.0, 5.0), (10.0, 5.0)], false).expect("path");
        assert!(!is_fillable(&p));
    }

    #[test]
    fn is_fillable_rejects_vertical_line() {
        let p = build(vec![(5.0, 0.0), (5.0, 10.0)], false).expect("path");
        assert!(!is_fillable(&p));
    }

    #[test]
    fn is_fillable_rejects_collapsed_closed_polygon() {
        // closed ring whose vertices all share the same y - typical of a tiny
        // polygon flattened onto a pixel row by world->pixel projection.
        let p = build(vec![(0.0, 7.0), (4.0, 7.0), (8.0, 7.0)], true).expect("path");
        assert!(!is_fillable(&p));
    }

    #[test]
    fn is_fillable_accepts_proper_polygon() {
        let p = build(vec![(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0)], true).expect("path");
        assert!(is_fillable(&p));
    }
}

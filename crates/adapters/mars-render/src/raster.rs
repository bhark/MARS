//! tiny-skia rasterisation helpers.

use mars_render_port::{Path as PortPath, RenderError};
use mars_style::{Colour, FillPaint, LabelStyle, LineCap as SLineCap, LineJoin as SLineJoin, Style};
use mars_text::{Fonts, GlyphMask};
use tiny_skia::{Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, StrokeDash, Transform};

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

/// dispatch on the `FillPaint` variant. Solid paints with the colour;
/// Hatch is implemented in a follow-up commit and is currently a no-op so
/// the stroke arm still runs (outline-only render is the conservative
/// fallback when the fill cannot be honoured).
fn draw_fill(pm: &mut Pixmap, path: &tiny_skia::Path, fill: FillPaint) {
    match fill {
        FillPaint::Solid(c) => {
            let mut paint = Paint::default();
            paint.set_color(colour_to_tsk(c));
            paint.anti_alias = true;
            pm.fill_path(path, &paint, FillRule::EvenOdd, Transform::identity(), None);
        }
        // hatch and other procedural paints land in the next commit.
        FillPaint::Hatch { .. } => {}
        // future FillPaint variants are forward-compatible no-ops.
        _ => {}
    }
}

/// draw a single styled path. uses even-odd fill rule (matches mapserver/qgis
/// expectations for self-intersecting symbol geometry; non-zero would change
/// the visual outcome of holes-as-CCW-rings produced upstream).
pub(crate) fn draw_path(pm: &mut Pixmap, path: &PortPath, style: &Style) {
    let Some(tsk_path) = build_path(path) else {
        return;
    };

    if let Some(fill) = style.fill
        && is_fillable(&tsk_path)
    {
        draw_fill(pm, &tsk_path, fill);
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
            paint.set_color(scaled_alpha(stroke_col, alpha_scale));
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

/// shape `text`, rasterise it once into an alpha mask, then composite the
/// mask into `pm` at `anchor` (baseline). when `style.halo` is set the mask
/// is stamped first in eight cardinal directions in the halo colour, then
/// the fill colour is laid on top.
pub(crate) fn draw_label(
    pm: &mut Pixmap,
    anchor: (f32, f32),
    text: &str,
    style: &LabelStyle,
    fonts: &Fonts,
) -> Result<(), RenderError> {
    let run = mars_text::measure(text, style, fonts).map_err(|e| RenderError::Backend(format!("font measure: {e}")))?;
    let mask = mars_text::rasterise(&run).map_err(|e| RenderError::Backend(format!("font rasterise: {e}")))?;
    if mask.coverage.is_empty() {
        return Ok(());
    }

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
                    composite_mask(pm, &mask, anchor, halo.colour, (dx as f32, dy as f32));
                }
            }
        }
    }

    composite_mask(pm, &mask, anchor, style.fill, (0.0, 0.0));
    Ok(())
}

/// `(x * y + 127) / 255` approximated as `(x*y + 0x80 + ((x*y) >> 8)) >> 8`,
/// the standard integer-/255 trick. error <= 1 LSB across the whole 0..=255
/// range; well inside font AA tolerance.
#[inline]
fn div255(v: u32) -> u32 {
    (v + 0x80 + (v >> 8)) >> 8
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

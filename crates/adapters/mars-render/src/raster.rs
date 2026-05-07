//! tiny-skia rasterisation helpers. SPEC §11.2.

use mars_render_port::{Path as PortPath, RenderError};
use mars_style::{Colour, LabelStyle, LineCap as SLineCap, LineJoin as SLineJoin, Style};
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

/// draw a single styled path. uses even-odd fill rule (matches mapserver/qgis
/// expectations for self-intersecting symbol geometry; non-zero would change
/// the visual outcome of holes-as-CCW-rings produced upstream).
pub(crate) fn draw_path(pm: &mut Pixmap, path: &PortPath, style: &Style) {
    let Some(tsk_path) = build_path(path) else {
        return;
    };

    if let Some(fill) = style.fill {
        let mut paint = Paint::default();
        paint.set_color(colour_to_tsk(fill));
        paint.anti_alias = true;
        pm.fill_path(&tsk_path, &paint, FillRule::EvenOdd, Transform::identity(), None);
    }

    if let Some(stroke_col) = style.stroke {
        let width = style.stroke_width.unwrap_or(1.0);
        if width > 0.0 {
            let mut paint = Paint::default();
            paint.set_color(colour_to_tsk(stroke_col));
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

fn composite_mask(pm: &mut Pixmap, mask: &GlyphMask, anchor: (f32, f32), colour: Colour, offset: (f32, f32)) {
    if mask.width == 0 || mask.height == 0 {
        return;
    }
    let pm_w = pm.width() as i32;
    let pm_h = pm.height() as i32;
    let dst_x0 = (anchor.0 + mask.origin_x as f32 + offset.0).round() as i32;
    let dst_y0 = (anchor.1 + mask.origin_y as f32 + offset.1).round() as i32;
    let data = pm.data_mut();
    let mw = mask.width as i32;
    let mh = mask.height as i32;
    let sr = colour.r;
    let sg = colour.g;
    let sb = colour.b;
    let sa = colour.a;
    for my in 0..mh {
        let dy = dst_y0 + my;
        if dy < 0 || dy >= pm_h {
            continue;
        }
        for mx in 0..mw {
            let dx = dst_x0 + mx;
            if dx < 0 || dx >= pm_w {
                continue;
            }
            let cov = mask.coverage[(my as u32 * mask.width + mx as u32) as usize];
            if cov == 0 {
                continue;
            }
            // src alpha = colour.a * coverage / 255
            let a_src = (u16::from(sa) * u16::from(cov) / 255) as u8;
            if a_src == 0 {
                continue;
            }
            let idx = ((dy * pm_w + dx) * 4) as usize;
            // premultiplied source RGBA
            let pr = (u16::from(sr) * u16::from(a_src) / 255) as u8;
            let pg = (u16::from(sg) * u16::from(a_src) / 255) as u8;
            let pb = (u16::from(sb) * u16::from(a_src) / 255) as u8;
            // premultiplied "over" composite onto existing pixel
            let inv = 255 - a_src;
            data[idx] = pr.saturating_add((u16::from(data[idx]) * u16::from(inv) / 255) as u8);
            data[idx + 1] = pg.saturating_add((u16::from(data[idx + 1]) * u16::from(inv) / 255) as u8);
            data[idx + 2] = pb.saturating_add((u16::from(data[idx + 2]) * u16::from(inv) / 255) as u8);
            data[idx + 3] = a_src.saturating_add((u16::from(data[idx + 3]) * u16::from(inv) / 255) as u8);
        }
    }
}

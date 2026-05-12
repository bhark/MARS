//! tiny-skia rasterisation helpers.

use mars_render_port::{Path as PortPath, RenderError};
use mars_style::{Colour, LabelStyle, Style};
use mars_text::{Fonts, GlyphMask};
use tiny_skia::Pixmap;

use crate::canvas::div255;
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


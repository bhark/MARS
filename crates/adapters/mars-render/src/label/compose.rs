//! glyph-mask compositor parameterised over a `Sampler`.
//!
//! both axis-aligned and rotated label stamps drive the same per-pixel blend
//! loop; the variations live in the sampler. `#[inline]` on the trait method
//! lets the compiler monomorphise to per-impl code, matching the codegen
//! quality of the prior hand-rolled loops.

use mars_style::Colour;
use mars_text::GlyphMask;
use tiny_skia::Pixmap;

use crate::canvas::div255;

/// axis-aligned mask stamp at `anchor + mask.origin + offset`. clips the dst
/// rect to canvas bounds so the `AxisSampler` can skip its OOB check.
pub(super) fn stamp_axis(pm: &mut Pixmap, mask: &GlyphMask, anchor: (f32, f32), colour: Colour, offset: (f32, f32)) {
    if mask.width == 0 || mask.height == 0 {
        return;
    }
    let pm_w = pm.width() as i32;
    let pm_h = pm.height() as i32;
    let dst_x0 = (anchor.0 + mask.origin_x as f32 + offset.0).round() as i32;
    let dst_y0 = (anchor.1 + mask.origin_y as f32 + offset.1).round() as i32;
    let mw = mask.width as i32;
    let mh = mask.height as i32;

    let dst_x_lo = dst_x0.max(0);
    let dst_x_hi = (dst_x0 + mw).min(pm_w);
    let dst_y_lo = dst_y0.max(0);
    let dst_y_hi = (dst_y0 + mh).min(pm_h);
    if dst_x_lo >= dst_x_hi || dst_y_lo >= dst_y_hi {
        return;
    }

    let sampler = AxisSampler { mask, dst_x0, dst_y0 };
    composite(pm, (dst_x_lo, dst_y_lo, dst_x_hi, dst_y_hi), colour, &sampler);
}

/// rotated mask stamp: rotate the four mask corners around `anchor` to bound
/// the dst rect, then run a `RotatedSampler` through the generic compositor.
/// `offset` is applied in the mask's local (pre-rotation) frame so halo
/// stamps rotate with the glyph rather than smearing outwards in canvas
/// space.
pub(super) fn stamp_rotated(
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
    let dst_x_lo = (minx.floor() as i32 - 1).max(0);
    let dst_x_hi = (maxx.ceil() as i32 + 1).min(pm_w);
    let dst_y_lo = (miny.floor() as i32 - 1).max(0);
    let dst_y_hi = (maxy.ceil() as i32 + 1).min(pm_h);

    let sampler = RotatedSampler {
        mask,
        anchor,
        origin: (ox, oy),
        cos: cos_a,
        sin: sin_a,
    };
    composite(pm, (dst_x_lo, dst_y_lo, dst_x_hi, dst_y_hi), colour, &sampler);
}

pub(super) trait Sampler {
    /// returns coverage at canvas (dx, dy). `None` when the canvas pixel maps
    /// outside the mask, `Some(0)` for transparent mask pixels. callers
    /// short-circuit on either.
    fn sample(&self, dx: i32, dy: i32) -> Option<u8>;
}

/// composite `colour` modulated by `sampler` coverage over the canvas
/// rectangle `(x_lo, y_lo, x_hi, y_hi)`. the rectangle must already be
/// clipped to the canvas bounds.
pub(super) fn composite<S: Sampler>(pm: &mut Pixmap, dst: (i32, i32, i32, i32), colour: Colour, sampler: &S) {
    let (x_lo, y_lo, x_hi, y_hi) = dst;
    if x_lo >= x_hi || y_lo >= y_hi {
        return;
    }
    let pm_w = pm.width() as usize;
    let data = pm.data_mut();
    let sr = u32::from(colour.r);
    let sg = u32::from(colour.g);
    let sb = u32::from(colour.b);
    let sa = u32::from(colour.a);

    for dy in y_lo..y_hi {
        let row_dst = dy as usize * pm_w * 4;
        for dx in x_lo..x_hi {
            let Some(cov) = sampler.sample(dx, dy) else { continue };
            if cov == 0 {
                continue;
            }
            let a_src = div255(sa * u32::from(cov));
            if a_src == 0 {
                continue;
            }
            let idx = row_dst + dx as usize * 4;
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

/// axis-aligned sampler: a direct (dx-dst_x0, dy-dst_y0) lookup. assumes the
/// caller has clipped the dst rect to lie inside the mask rect.
pub(super) struct AxisSampler<'a> {
    pub mask: &'a GlyphMask,
    pub dst_x0: i32,
    pub dst_y0: i32,
}

impl Sampler for AxisSampler<'_> {
    #[inline]
    fn sample(&self, dx: i32, dy: i32) -> Option<u8> {
        let mx = (dx - self.dst_x0) as usize;
        let my = (dy - self.dst_y0) as usize;
        let stride = self.mask.width as usize;
        Some(self.mask.coverage[my * stride + mx])
    }
}

/// rotated sampler: inverse-rotate around `anchor` and look up in mask-local
/// (origin-relative) coords. nearest-neighbour sampling; aliasing is
/// acceptable at the small font sizes that drive line labels.
pub(super) struct RotatedSampler<'a> {
    pub mask: &'a GlyphMask,
    pub anchor: (f32, f32),
    /// mask.origin + offset, in canvas-pixel units.
    pub origin: (f32, f32),
    pub cos: f32,
    pub sin: f32,
}

impl Sampler for RotatedSampler<'_> {
    #[inline]
    fn sample(&self, dx: i32, dy: i32) -> Option<u8> {
        let rx = dx as f32 - self.anchor.0;
        let ry = dy as f32 - self.anchor.1;
        let lx = self.cos * rx + self.sin * ry;
        let ly = -self.sin * rx + self.cos * ry;
        let mx = (lx - self.origin.0).floor() as i32;
        let my = (ly - self.origin.1).floor() as i32;
        let mw = self.mask.width as i32;
        let mh = self.mask.height as i32;
        if mx < 0 || my < 0 || mx >= mw || my >= mh {
            return None;
        }
        Some(self.mask.coverage[my as usize * mw as usize + mx as usize])
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_text::GlyphMask;

    fn mask3x3() -> GlyphMask {
        // a 3x3 mask with a non-trivial coverage pattern; origin (0,0).
        GlyphMask {
            width: 3,
            height: 3,
            origin_x: 0,
            origin_y: 0,
            coverage: vec![10, 20, 30, 40, 50, 60, 70, 80, 90],
        }
    }

    #[test]
    fn axis_sampler_parity_with_raw_mask_read() {
        let mask = mask3x3();
        let s = AxisSampler {
            mask: &mask,
            dst_x0: 5,
            dst_y0: 7,
        };
        for my in 0..3i32 {
            for mx in 0..3i32 {
                let raw = mask.coverage[(my * 3 + mx) as usize];
                let got = s.sample(5 + mx, 7 + my).expect("inside");
                assert_eq!(got, raw, "axis sampler mismatch at ({mx},{my})");
            }
        }
    }

    #[test]
    fn rotated_sampler_zero_degrees_matches_axis() {
        let mask = mask3x3();
        let axis = AxisSampler {
            mask: &mask,
            dst_x0: 4,
            dst_y0: 9,
        };
        let rot = RotatedSampler {
            mask: &mask,
            anchor: (4.0, 9.0),
            origin: (0.0, 0.0),
            cos: 1.0,
            sin: 0.0,
        };
        for my in 0..3i32 {
            for mx in 0..3i32 {
                let dx = 4 + mx;
                let dy = 9 + my;
                let a = axis.sample(dx, dy).expect("axis inside");
                let r = rot.sample(dx, dy).expect("rotated inside");
                assert_eq!(a, r, "0-deg rotated must match axis at ({mx},{my})");
            }
        }
    }

    #[test]
    fn rotated_sampler_ninety_degrees_round_trip() {
        // 90 ccw rotation around anchor at (0,0). canvas (dx, dy) maps via
        // inverse rotation to mask coords. for cos=0, sin=1: lx = sin*dy, ly = -sin*dx
        // wait: lx = cos*rx + sin*ry = 0*rx + 1*ry = ry = dy; ly = -sin*rx + cos*ry = -rx = -dx
        // so canvas (dx, dy) -> mask (dy, -dx). pick a pixel and verify.
        let mask = mask3x3();
        let rot = RotatedSampler {
            mask: &mask,
            anchor: (0.0, 0.0),
            origin: (0.0, 0.0),
            cos: 0.0,
            sin: 1.0,
        };
        // canvas (dy=2, dx=0) -> mask (2, 0) = coverage[2] = 30
        assert_eq!(rot.sample(0, 2), Some(30));
        // canvas (dx=0, dy=0) -> mask (0, 0) = coverage[0] = 10
        assert_eq!(rot.sample(0, 0), Some(10));
        // out of mask
        assert_eq!(rot.sample(5, 5), None);
    }
}

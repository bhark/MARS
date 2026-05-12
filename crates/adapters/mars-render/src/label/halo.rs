//! halo: stamp the glyph mask in a ring of integer offsets at the halo
//! colour, before the main fill stamp lays on top.
//!
//! the user-facing feature stays named "halo"; the implementation strategy
//! (offset stamping vs distance-field mask vs real outline) can evolve
//! inside this file without renaming.

use mars_style::Halo;
use mars_text::GlyphMask;
use tiny_skia::Pixmap;

use super::compose;

pub(crate) fn stamp(pm: &mut Pixmap, mask: &GlyphMask, anchor: (f32, f32), halo: &Halo, angle_rad: f32) {
    let radius = halo.width.max(0.0).round() as i32;
    if radius == 0 {
        return;
    }
    let axis_aligned = angle_rad.abs() < f32::EPSILON;
    // 8-direction offset stamp at unit step. wider halos repeat the stamp at
    // integer offsets up to `radius`. simple but cheap; the perceptual budget
    // on labelled goldens absorbs the AA jitter.
    for dx in -radius..=radius {
        for dy in -radius..=radius {
            if dx == 0 && dy == 0 {
                continue;
            }
            if dx * dx + dy * dy > radius * radius {
                continue;
            }
            if axis_aligned {
                compose::stamp_axis(pm, mask, anchor, halo.colour, (dx as f32, dy as f32));
            } else {
                compose::stamp_rotated(pm, mask, anchor, halo.colour, (dx as f32, dy as f32), angle_rad);
            }
        }
    }
}

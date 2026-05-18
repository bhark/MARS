//! ANGLE FOLLOW: place each glyph along a polyline, rotated to its own
//! local tangent. distinct from the block-rotated `label::draw` path, which
//! rotates the whole glyph mask as a unit.
//!
//! the runtime hands us a pixel-space polyline that's already projected and
//! CRS-transformed plus a starting arc-length. we shape the run once,
//! sample the polyline at each glyph's centre arc, and stamp each glyph
//! through the existing `compose::stamp_rotated` path.

use mars_render_port::RenderError;
use mars_style::ResolvedLabelStyle;
use mars_text::{Fonts, GlyphMask};
use tiny_skia::Pixmap;

use super::compose;
use super::halo;
use crate::polyline::{cumulative_arc_length, sample_at};

pub(crate) fn draw_follow(
    pm: &mut Pixmap,
    polyline: &[(f32, f32)],
    start_arc: f32,
    text: &str,
    style: &ResolvedLabelStyle,
    fonts: &Fonts,
) -> Result<(), RenderError> {
    if polyline.len() < 2 {
        return Ok(());
    }
    let run = mars_text::measure(text, style, fonts).map_err(|e| RenderError::Backend(format!("font measure: {e}")))?;
    if run.glyph_count() == 0 {
        return Ok(());
    }
    let cum = cumulative_arc_length(polyline);
    let arc_total = match cum.last().copied() {
        Some(t) if t > 0.0 => t,
        _ => return Ok(()),
    };

    // pre-shape glyph rasterisations + arc positions. we walk the same
    // schedule twice when a halo is set so that the halo lands behind every
    // glyph in a separate pass, matching `label::draw`'s ordering.
    let glyphs: Vec<_> = run
        .glyphs()
        .enumerate()
        .filter_map(|(idx, g)| {
            let centre_arc = start_arc + g.x + g.advance_x * 0.5;
            if !(0.0..=arc_total).contains(&centre_arc) {
                return None;
            }
            let sample = sample_at(polyline, &cum, centre_arc)?;
            let mask = mars_text::rasterise_glyph(&run, idx)
                .map_err(|e| RenderError::Backend(format!("font rasterise: {e}")))
                .ok()?;
            if mask.coverage.is_empty() {
                return None;
            }
            // keep text reading left-to-right: when the local tangent
            // points into the left half-plane, flip the glyph by pi.
            let mut angle = sample.tangent.1.atan2(sample.tangent.0);
            if !(-std::f32::consts::FRAC_PI_2..=std::f32::consts::FRAC_PI_2).contains(&angle) {
                angle += std::f32::consts::PI;
                if angle > std::f32::consts::PI {
                    angle -= std::f32::consts::TAU;
                }
            }
            Some(GlyphPlacement {
                mask,
                anchor: sample.pos,
                angle,
            })
        })
        .collect();

    if let Some(h) = &style.halo {
        for g in &glyphs {
            halo::stamp(pm, &g.mask, g.anchor, h, g.angle);
        }
    }
    for g in &glyphs {
        compose::stamp_rotated(pm, &g.mask, g.anchor, style.fill, (0.0, 0.0), g.angle);
    }
    Ok(())
}

struct GlyphPlacement {
    mask: GlyphMask,
    anchor: (f32, f32),
    angle: f32,
}

#[cfg(test)]
mod tests;

//! style normalisation: condense an Option-heavy `Style` into a non-Option,
//! validated view that drives the fill and stroke pipelines.
//!
//! adding a new `Style` field touches `resolve` in one place; downstream code
//! reads from `Resolved*` instead of re-doing the Option dance per call site.

use mars_style::{FillPaint, ResolvedStyle};
use tiny_skia::{BlendMode, LineCap, LineJoin, StrokeDash};

use crate::canvas::{map_blend, map_cap, map_join};
use crate::stroke;

#[derive(Debug)]
pub(crate) struct Resolved {
    pub fill: Option<ResolvedFill>,
    pub stroke: Option<ResolvedStroke>,
    /// per-pass compositing operator translated from
    /// [`mars_style::BlendMode`]. `SourceOver` is the rasteriser default and
    /// is passed to tiny-skia as `None` at draw-call time.
    pub blend_mode: BlendMode,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedFill {
    pub paint: FillPaint,
    /// 0..=1; style.opacity baked in. multiplied into the paint colour's
    /// alpha by the fill pipeline.
    pub alpha: f32,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedStroke {
    pub colour: mars_style::Colour,
    /// 0..=1; style.opacity and the sub-pixel-stroke alpha-scale (AGG-style
    /// emulation: widths <1 paint a 1px stroke at proportional alpha) folded
    /// together.
    pub alpha: f32,
    /// final tiny-skia stroke width: clamped up to 1.0 if the requested
    /// width was a sub-pixel value (the alpha-scale carries the visual
    /// intensity instead).
    pub width: f32,
    pub cap: LineCap,
    pub join: LineJoin,
    pub dash: Option<StrokeDash>,
    /// 0 if unset; positive = right of direction of travel in y-down pixel
    /// space.
    pub offset_px: f32,
    /// stamped-marker repeat policy. `Some` only when `style.marker` is also
    /// set; a gap with no marker has nothing to stamp.
    pub gap: Option<ResolvedStrokeGap>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ResolvedStrokeGap {
    pub interval_px: f32,
    pub initial_px: f32,
}

pub(crate) fn resolve(style: &ResolvedStyle) -> Resolved {
    let opacity = style.opacity.unwrap_or(1.0).clamp(0.0, 1.0);
    let blend_mode = style.blend_mode.map(map_blend).unwrap_or(BlendMode::SourceOver);

    let fill = style.fill.clone().map(|paint| ResolvedFill { paint, alpha: opacity });

    let stroke = style.stroke.and_then(|colour| {
        let requested = style.stroke_width.unwrap_or(1.0);
        if !(requested.is_finite() && requested > 0.0) {
            return None;
        }
        let (width, alpha_scale) = if requested < 1.0 {
            (1.0, requested)
        } else {
            (requested, 1.0)
        };
        let dash = style.stroke_dasharray.as_deref().and_then(stroke::dash::build);
        let offset_px = match style.stroke_offset_px {
            Some(d) if d.is_finite() && d.abs() > f32::EPSILON => d,
            _ => 0.0,
        };
        // marker absent -> nothing to stamp; treat as no-op. defensive
        // bounds match the config validator so the renderer never sees
        // a degenerate interval.
        let gap = style.stroke_gap.and_then(|g| {
            style.marker.as_ref()?;
            if !(g.interval_px.is_finite() && g.interval_px > 0.0) {
                return None;
            }
            let initial_px = if g.initial_px.is_finite() && g.initial_px >= 0.0 {
                g.initial_px
            } else {
                0.0
            };
            Some(ResolvedStrokeGap {
                interval_px: g.interval_px,
                initial_px,
            })
        });
        Some(ResolvedStroke {
            colour,
            alpha: alpha_scale * opacity,
            width,
            cap: style.stroke_linecap.map(map_cap).unwrap_or(LineCap::Butt),
            join: style.stroke_linejoin.map(map_join).unwrap_or(LineJoin::Miter),
            dash,
            offset_px,
            gap,
        })
    });

    Resolved {
        fill,
        stroke,
        blend_mode,
    }
}

#[cfg(test)]
mod tests;

//! canvas-level paint helpers: background fill, colour mapping, alpha scaling,
//! cap/join translation, and the `div255` 8-bit alpha-blend primitive.

use mars_style::{Colour, LineCap as SLineCap, LineJoin as SLineJoin};
use tiny_skia::{Color, LineCap, LineJoin, Pixmap};

pub(crate) fn colour_to_tsk(c: Colour) -> Color {
    Color::from_rgba8(c.r, c.g, c.b, c.a)
}

/// returns `c` with alpha multiplied by `scale` (clamped to [0,1]). used to
/// emulate AGG sub-pixel stroke widths: a width of 0.15 renders as a 1px
/// stroke at 15% alpha rather than a full-intensity 1px line.
pub(crate) fn scaled_alpha(c: Colour, scale: f32) -> Color {
    let s = scale.clamp(0.0, 1.0);
    let a = ((c.a as f32) * s).round().clamp(0.0, 255.0) as u8;
    Color::from_rgba8(c.r, c.g, c.b, a)
}

/// returns `c` with alpha multiplied by `scale` (clamped to [0,1]). same idea
/// as `scaled_alpha` but yields a `Colour` so callers can re-thread the
/// result through `FillPaint::Hatch::colour` or further-scaled paths.
pub(crate) fn scaled_alpha_colour(c: Colour, scale: f32) -> Colour {
    let s = scale.clamp(0.0, 1.0);
    let a = ((c.a as f32) * s).round().clamp(0.0, 255.0) as u8;
    Colour {
        r: c.r,
        g: c.g,
        b: c.b,
        a,
    }
}

pub(crate) fn map_cap(c: SLineCap) -> LineCap {
    match c {
        SLineCap::Butt => LineCap::Butt,
        SLineCap::Round => LineCap::Round,
        SLineCap::Square => LineCap::Square,
    }
}

pub(crate) fn map_join(j: SLineJoin) -> LineJoin {
    match j {
        SLineJoin::Miter => LineJoin::Miter,
        SLineJoin::Round => LineJoin::Round,
        SLineJoin::Bevel => LineJoin::Bevel,
    }
}

/// translate the port-level [`mars_style::BlendMode`] into the equivalent
/// tiny-skia blend mode. `BlendMode::SourceOver` is the rasteriser's default
/// and round-trips back to a `None` parameter at draw-call time.
pub(crate) fn map_blend(b: mars_style::BlendMode) -> tiny_skia::BlendMode {
    use mars_style::BlendMode;
    match b {
        BlendMode::SourceOver => tiny_skia::BlendMode::SourceOver,
        BlendMode::Multiply => tiny_skia::BlendMode::Multiply,
        BlendMode::Screen => tiny_skia::BlendMode::Screen,
        BlendMode::Overlay => tiny_skia::BlendMode::Overlay,
        BlendMode::Darken => tiny_skia::BlendMode::Darken,
        BlendMode::Lighten => tiny_skia::BlendMode::Lighten,
    }
}

/// fill the pixmap with a solid colour (used for canvas background).
pub(crate) fn fill_background(pm: &mut Pixmap, c: Colour) {
    pm.fill(colour_to_tsk(c));
}

/// `(x * y + 127) / 255` approximated as `(x*y + 0x80 + ((x*y) >> 8)) >> 8`,
/// the standard integer-/255 trick. error <= 1 LSB across the whole 0..=255
/// range; well inside font AA tolerance.
#[inline]
pub(crate) fn div255(v: u32) -> u32 {
    (v + 0x80 + (v >> 8)) >> 8
}

#[cfg(test)]
mod tests;

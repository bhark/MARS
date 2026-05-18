//! stamped-marker-along-line. resamples each subpath at fixed arc-length
//! intervals, rotates the parent style's marker to the local tangent at
//! each sample, and dispatches to the existing marker render path. mirrors
//! mapserver's `GAP` / `INITIALGAP`.

use mars_render_port::{Path as PortPath, RenderError};
use mars_style::{ResolvedMarker, ResolvedStyle};
use mars_text::Fonts;
use tiny_skia::Pixmap;

use crate::polyline::{cumulative_arc_length, sample_at};
use crate::prepare::ResolvedStrokeGap;
use crate::symbol;

/// stamp `marker` along each subpath of `port_path`. caller must guarantee
/// `gap.interval_px > 0`; `prepare::resolve` enforces this. `style` carries
/// the marker's fill / stroke / opacity into the recursive symbol dispatch.
pub(crate) fn stamp(
    pm: &mut Pixmap,
    port_path: &PortPath,
    marker: &ResolvedMarker,
    style: &ResolvedStyle,
    gap: ResolvedStrokeGap,
    fonts: &Fonts,
) -> Result<(), RenderError> {
    // drop stroke + stroke_gap on the marker-stamp pass: a marker can carry
    // its own outline through `style.stroke`, but re-stamping along a tiny
    // marker subpath would recurse infinitely. clearing gap is the
    // termination invariant; clearing stroke matches mapserver's mental
    // model where GAP styles paint only the marker glyph.
    let mut marker_style = style.clone();
    marker_style.stroke_gap = None;
    marker_style.stroke = None;
    marker_style.marker = Some(marker.clone());

    for sub in &port_path.subpaths {
        if sub.points.len() < 2 {
            continue;
        }
        let cum = cumulative_arc_length(&sub.points);
        let total = cum.last().copied().unwrap_or(0.0);
        if !(total.is_finite() && total > 0.0) {
            continue;
        }

        let mut t = gap.initial_px;
        while t <= total {
            if let Some(sample) = sample_at(&sub.points, &cum, t) {
                let angle = sample.tangent.1.atan2(sample.tangent.0);
                symbol::dispatch(pm, sample.pos, angle, &marker_style, fonts)?;
            }
            t += gap.interval_px;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

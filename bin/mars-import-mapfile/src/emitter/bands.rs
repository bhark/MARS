//! scale-band ladder math used by `render` to split per-layer sources into
//! per-band tiers.

use tracing::warn;

use super::skeleton::{LayerSkeleton, SourceSkeleton};

/// default scale-band ladder used when `--bands` is not supplied.
/// caps are denom upper bounds (exclusive). the overview cap is finite
/// (1:10_000_000) - large enough for a country-wide view, small enough to
/// render cleanly in YAML; operators that need a wider ladder pass `--bands`.
pub(crate) fn default_bands() -> Vec<(String, u64)> {
    vec![
        ("detail".into(), 2_500),
        ("hi".into(), 12_500),
        ("mid".into(), 50_000),
        ("lo".into(), 250_000),
        ("overview".into(), 10_000_000),
    ]
}

/// expand an ordered ladder of caps into bands carrying their lower bound too.
/// band i covers `[prev_cap, cap)`; band 0's lower bound is 0.
pub(super) struct BandWindow<'a> {
    pub(super) name: &'a str,
    pub(super) min: u64,
    pub(super) cap: u64,
}

pub(super) fn band_windows(bands: &[(String, u64)]) -> Vec<BandWindow<'_>> {
    let mut out = Vec::with_capacity(bands.len());
    let mut prev: u64 = 0;
    for (name, cap) in bands {
        out.push(BandWindow {
            name: name.as_str(),
            min: prev,
            cap: *cap,
        });
        prev = *cap;
    }
    out
}

/// per-tier emission inside a single band for a single layer.
pub(super) struct EmittedTier<'a> {
    pub(super) src: &'a SourceSkeleton,
    /// `None` = last tier of this band (no `max_denom_exclusive` rendered).
    pub(super) max_denom: Option<u64>,
}

/// for each band, compute the tier-set this layer contributes.
/// returns `(band_name, Vec<EmittedTier>)` per band that the layer fully covers.
/// bands the layer only partially covers are dropped with a warn.
pub(super) fn split_layer_into_bands<'a>(
    layer: &'a LayerSkeleton,
    windows: &[BandWindow<'a>],
) -> Vec<(&'a str, Vec<EmittedTier<'a>>)> {
    if layer.sources.is_empty() {
        return Vec::new();
    }

    // contiguous source intervals within a layer: [prev_max, this_max).
    // first source starts at 0; an open-ended `max_denom_exclusive` is u64::MAX.
    let mut intervals: Vec<(u64, u64, &SourceSkeleton)> = Vec::with_capacity(layer.sources.len());
    let mut prev: u64 = 0;
    for src in &layer.sources {
        let this = src.max_denom_exclusive.unwrap_or(u64::MAX);
        if this <= prev {
            warn!(
                layer = %layer.name,
                prev_max = prev,
                this_max = this,
                "layer sources not in strictly increasing max_denom order; skipping later tier"
            );
            continue;
        }
        intervals.push((prev, this, src));
        prev = this;
    }
    if intervals.is_empty() {
        return Vec::new();
    }
    let layer_min = intervals.first().map(|(m, _, _)| *m).unwrap_or(0);
    let layer_max = intervals.last().map(|(_, m, _)| *m).unwrap_or(0);

    let mut out: Vec<(&str, Vec<EmittedTier>)> = Vec::new();
    for w in windows {
        // skip bands the layer doesn't intersect at all.
        if w.cap <= layer_min || w.min >= layer_max {
            continue;
        }
        // partial coverage: layer doesn't fully span [w.min, w.cap).
        if layer_min > w.min || layer_max < w.cap {
            warn!(
                layer = %layer.name,
                band = %w.name,
                band_min = w.min,
                band_cap = w.cap,
                layer_min,
                layer_max,
                "layer partially overlaps band; dropping (validator requires full band coverage)"
            );
            continue;
        }

        // collect the source intervals that intersect this band.
        let in_band: Vec<&(u64, u64, &SourceSkeleton)> = intervals
            .iter()
            .filter(|(lo, hi, _)| *hi > w.min && *lo < w.cap)
            .collect();

        let n = in_band.len();
        let mut tiers: Vec<EmittedTier> = Vec::with_capacity(n);
        for (idx, (_lo, hi, src)) in in_band.iter().enumerate() {
            let is_last = idx + 1 == n;
            let effective = (*hi).min(w.cap);
            let max_denom = if is_last && effective == w.cap {
                None
            } else {
                Some(effective)
            };
            tiers.push(EmittedTier { src, max_denom });
        }
        out.push((w.name, tiers));
    }
    out
}

#[cfg(test)]
mod tests;

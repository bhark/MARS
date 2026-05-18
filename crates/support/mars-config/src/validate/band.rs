use std::collections::{BTreeMap, BTreeSet};

use crate::ConfigError;
use crate::model::{Config, Layer, ScaleWindow, SourceBinding};

/// Walk `config.scales.bands` once, rejecting duplicates and building the
/// derived half-open scale window for each band. The window map is consumed
/// downstream by per-layer tier validation; the name set by per-source
/// band-reference checks.
pub(super) fn validate_bands(config: &Config) -> Result<BandIndex, ConfigError> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    let mut windows: BTreeMap<String, ScaleWindow> = BTreeMap::new();
    let mut prev_max: Option<u64> = None;
    for band in &config.scales.bands {
        if !names.insert(band.name.clone()) {
            return Err(ConfigError::Invalid(format!(
                "duplicate band name {:?} in scales.bands",
                band.name
            )));
        }
        windows.insert(
            band.name.clone(),
            ScaleWindow {
                min: prev_max,
                max: Some(band.max_denom),
            },
        );
        prev_max = Some(band.max_denom);
    }
    Ok(BandIndex { names, windows })
}

pub(super) struct BandIndex {
    pub names: BTreeSet<String>,
    pub windows: BTreeMap<String, ScaleWindow>,
}

/// Intersect two half-open scale windows. `None` bounds act as ±infinity.
/// Returns `None` if the intersection is empty (lo >= hi).
pub(super) fn intersect_scale_windows(a: &ScaleWindow, b: &ScaleWindow) -> Option<ScaleWindow> {
    let min = match (a.min, b.min) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    };
    let max = match (a.max, b.max) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    };
    if let (Some(lo), Some(hi)) = (min, max)
        && lo >= hi
    {
        return None;
    }
    Some(ScaleWindow { min, max })
}

/// Validate tier-set rules for every (layer, band) that has more than one
/// source or any source carrying `max_denom_exclusive`.
pub(super) fn validate_band_tiers(
    layer: &Layer,
    band_windows: &BTreeMap<String, ScaleWindow>,
) -> Result<(), ConfigError> {
    let mut by_band: BTreeMap<&str, Vec<(usize, &SourceBinding)>> = BTreeMap::new();
    for (i, binding) in layer.sources.iter().enumerate() {
        if let Some(band) = binding.band.as_deref() {
            by_band.entry(band).or_default().push((i, binding));
        }
    }

    for (band_name, sources) in by_band {
        let band_window = band_windows.get(band_name).ok_or_else(|| {
            ConfigError::Invalid(format!(
                "layer {} band {band_name:?} not declared in scales.bands",
                layer.name
            ))
        })?;
        let band_cap = band_window
            .max
            .ok_or_else(|| ConfigError::Invalid("band cap is missing".into()))?;

        // back-compat: single source, no max_denom → covers whole band, no further checks.
        if sources.len() == 1 && sources[0].1.max_denom.is_none() {
            continue;
        }

        let mut prev_max: Option<u64> = None;
        for (idx, (i, binding)) in sources.iter().enumerate() {
            let is_last = idx == sources.len() - 1;
            let this_max = binding.max_denom;

            if !is_last && this_max.is_none() {
                return Err(ConfigError::Invalid(format!(
                    "layer {} source[{i}] in band {band_name:?} omits max_denom_exclusive but is not the last tier",
                    layer.name
                )));
            }

            if let Some(m) = this_max {
                if m == 0 {
                    return Err(ConfigError::Invalid(format!(
                        "layer {} source[{i}] in band {band_name:?} max_denom_exclusive must be > 0",
                        layer.name
                    )));
                }
                if m > band_cap {
                    return Err(ConfigError::Invalid(format!(
                        "layer {} source[{i}] in band {band_name:?} max_denom_exclusive ({m}) exceeds band cap ({band_cap})",
                        layer.name
                    )));
                }
                if idx == 0
                    && let Some(band_min) = band_window.min
                    && m <= band_min
                {
                    return Err(ConfigError::Invalid(format!(
                        "layer {} source[{i}] in band {band_name:?} max_denom_exclusive ({m}) is not strictly greater than band lower bound ({band_min})",
                        layer.name
                    )));
                }
                if let Some(p) = prev_max
                    && m <= p
                {
                    return Err(ConfigError::Invalid(format!(
                        "layer {} source[{i}] in band {band_name:?} max_denom_exclusive ({m}) is not strictly greater than previous tier ({p})",
                        layer.name
                    )));
                }
            }

            prev_max = this_max;
        }

        // last tier must reach band cap (or omit, which is equivalent).
        let last_max = sources.last().and_then(|(_, b)| b.max_denom);
        if let Some(m) = last_max
            && m != band_cap
        {
            return Err(ConfigError::Invalid(format!(
                "layer {} last tier in band {band_name:?} max_denom_exclusive ({m}) does not equal band cap ({band_cap})",
                layer.name
            )));
        }
    }

    Ok(())
}

/// Fold each source binding's declared `band` into its `scale` window.
/// When multiple sources share a band, they form a tier-set: each tier's
/// half-open window is `[prev_tier_max, this_tier_max)` intersected with the
/// band window and any explicit `scale` bound.
pub(super) fn resolve_band_routing(config: &mut Config) -> Result<(), ConfigError> {
    let mut band_windows: BTreeMap<String, ScaleWindow> = BTreeMap::new();
    let mut prev_max: Option<u64> = None;
    for band in &config.scales.bands {
        band_windows.insert(
            band.name.clone(),
            ScaleWindow {
                min: prev_max,
                max: Some(band.max_denom),
            },
        );
        prev_max = Some(band.max_denom);
    }

    for layer in &mut config.layers {
        // collect (band, idx) pairs for mutable indexing later.
        let mut by_band: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (idx, source) in layer.sources.iter().enumerate() {
            if let Some(band) = source.band.clone() {
                by_band.entry(band).or_default().push(idx);
            }
        }

        for (band_name, indices) in by_band {
            let band_window = band_windows.get(&band_name).ok_or_else(|| {
                ConfigError::Invalid(format!(
                    "layer {} band {band_name:?} not declared in scales.bands",
                    layer.name
                ))
            })?;

            let mut prev_tier_max: Option<u64> = None;
            for idx in indices {
                let source = &mut layer.sources[idx];
                let tier_min = prev_tier_max.or(band_window.min);
                let tier_max = source.max_denom.or(band_window.max);

                let tier_window = ScaleWindow {
                    min: tier_min,
                    max: tier_max,
                };
                let resolved = match &source.scale {
                    None => tier_window,
                    Some(explicit) => intersect_scale_windows(explicit, &tier_window).ok_or_else(|| {
                        ConfigError::Invalid(format!(
                            "layer {} source from {:?} explicit scale window {:?} is disjoint from tier window {:?}",
                            layer.name,
                            source.source_descriptor(),
                            explicit,
                            tier_window
                        ))
                    })?,
                };
                source.scale = Some(resolved);
                prev_tier_max = tier_max;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests;

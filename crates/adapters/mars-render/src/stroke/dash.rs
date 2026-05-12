//! stroke-dash construction.
//!
//! odd-length arrays would silently render solid in tiny-skia; warn once so
//! style authors notice the typo.

use tiny_skia::StrokeDash;

pub(crate) fn build(dashes: &[f32]) -> Option<StrokeDash> {
    if dashes.is_empty() {
        return None;
    }
    let built = StrokeDash::new(dashes.to_vec(), 0.0);
    if built.is_none() {
        tracing::warn!(dashes = ?dashes, "invalid stroke dash array: odd length, rendering solid");
    }
    built
}

use std::collections::BTreeMap;
use std::collections::HashSet;

use crate::ConfigError;
use crate::model::{Reprojection, Source, TileMatrixSet};

/// Cross-cutting reprojection-allowlist coherence check, run on the composed
/// `Config` only. For every declared tile-matrix-set CRS, either at least one
/// source's `native_crs` already matches (no reprojection needed for that TMS),
/// or the TMS CRS appears in `reprojection.allowlist`. Otherwise the renderer
/// would have to reproject to a CRS the operator has not opted into.
///
/// Pure read; takes the union of "advertised output CRSes" from the WMTS
/// tile-matrix-set table. WMS output CRSes are not enumerated up-front - the
/// `reprojection.allowlist` is itself the WMS opt-in list at request time,
/// so there is no separate WMS coherence check here.
pub(super) fn validate_reprojection_coherence(
    reprojection: &Reprojection,
    tile_matrix_sets: &BTreeMap<String, TileMatrixSet>,
    sources: &[Source],
) -> Result<(), ConfigError> {
    if tile_matrix_sets.is_empty() {
        return Ok(());
    }
    let native: HashSet<&str> = sources.iter().map(|s| s.native_crs.as_str()).collect();
    let allowed: HashSet<&str> = reprojection.allowlist.iter().map(|c| c.as_str()).collect();
    for (name, tms) in tile_matrix_sets {
        let crs = tms.crs.as_str();
        if native.contains(crs) {
            continue;
        }
        if !allowed.contains(crs) {
            return Err(ConfigError::Invalid(format!(
                "tile_matrix_sets[{name:?}] crs {crs:?} does not match any source.native_crs and is missing \
                 from reprojection.allowlist; either add it to the allowlist or declare a source with that \
                 native CRS"
            )));
        }
    }
    Ok(())
}

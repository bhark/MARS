use crate::ArtifactError;

use super::{
    FeatureGeom, GeomKind,
    codec::read_geom,
    index::{FeatureIndexEntry, iter_feature_index},
};

/// Decode the coordinates for a single index entry. The `coord_area` slice
/// must be the one returned by `FeatureIndexIter::coord_area`.
pub fn decode_one_geom(coord_area: &[u8], entry: &FeatureIndexEntry) -> Result<GeomKind, ArtifactError> {
    let off = entry.coord_offset as usize;
    let end = off
        .checked_add(entry.coord_len as usize)
        .ok_or(ArtifactError::Truncated)?;
    if end > coord_area.len() {
        return Err(ArtifactError::Truncated);
    }
    let mut pos = off;
    let geom = read_geom(entry.geom_type, coord_area, &mut pos)?;
    if pos != end {
        return Err(ArtifactError::Malformed("coord block length mismatch"));
    }
    Ok(geom)
}

/// Decode only features whose `(user_id, bbox)` predicate returns `true`.
/// Walks the cheap index, applies `pred`, and decodes coordinates only for
/// survivors. Equivalent in result to filtering the output of
/// [`decode_geometry_payload`], but avoids decoding skipped features.
pub fn decode_geometry_payload_filtered<F>(bytes: &[u8], mut pred: F) -> Result<Vec<FeatureGeom>, ArtifactError>
where
    F: FnMut(u64, [f32; 4]) -> bool,
{
    let iter = iter_feature_index(bytes)?;
    let coord_area = iter.coord_area();
    let mut out = Vec::new();
    for entry in iter {
        let entry = entry?;
        if !pred(entry.user_id, entry.bbox) {
            continue;
        }
        let geom = decode_one_geom(coord_area, &entry)?;
        out.push(FeatureGeom {
            user_id: entry.user_id,
            bbox: entry.bbox,
            geom,
        });
    }
    Ok(out)
}

/// Decode features at the given slot positions in the geometry payload.
///
/// `slots` is the set of feature indices returned by
/// [`crate::SpatialIndex::query`]; entries are the position in the
/// payload's id-sorted feature index (the same `idx` the snapshot writer
/// passed to `SpatialIndexBuilder::add`). Out-of-range slots are silently
/// dropped - they cannot match any feature.
///
/// `slots` need not be sorted or deduped. The function sorts a local copy
/// and walks the index once.
pub fn decode_geometry_at_slots(bytes: &[u8], slots: &[u32]) -> Result<Vec<FeatureGeom>, ArtifactError> {
    if slots.is_empty() {
        return Ok(Vec::new());
    }
    let mut sorted = slots.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let iter = iter_feature_index(bytes)?;
    let coord_area = iter.coord_area();
    let mut out: Vec<FeatureGeom> = Vec::with_capacity(sorted.len());
    let mut cursor = 0usize;
    for (slot_idx, entry) in iter.enumerate() {
        let entry = entry?;
        if cursor >= sorted.len() {
            break;
        }
        let want = sorted[cursor];
        let slot_u32 = u32::try_from(slot_idx).map_err(|_| ArtifactError::Malformed("slot index overflow"))?;
        if slot_u32 != want {
            continue;
        }
        cursor += 1;
        let geom = decode_one_geom(coord_area, &entry)?;
        out.push(FeatureGeom {
            user_id: entry.user_id,
            bbox: entry.bbox,
            geom,
        });
    }
    Ok(out)
}

pub fn decode_geometry_payload(bytes: &[u8]) -> Result<Vec<FeatureGeom>, ArtifactError> {
    let iter = iter_feature_index(bytes)?;
    let coord_area = iter.coord_area();
    let mut out = Vec::with_capacity(iter.len());
    for entry in iter {
        let entry = entry?;
        let geom = decode_one_geom(coord_area, &entry)?;
        out.push(FeatureGeom {
            user_id: entry.user_id,
            bbox: entry.bbox,
            geom,
        });
    }
    Ok(out)
}

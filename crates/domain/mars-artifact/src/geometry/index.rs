use crate::ArtifactError;

use super::codec::read_array;
use super::{GT_LINESTRING, GT_MULTILINESTRING, GT_MULTIPOINT, GT_MULTIPOLYGON, GT_POINT, GT_POLYGON, GeomType};

/// Length in bytes of one feature index entry.
///
/// Layout: u64 user_id, [f32; 4] bbox, u8 geom_type, u32 coord_offset, u32 coord_len.
/// `user_id` is non-key data (a source row may produce multiple features that
/// share the same user_id). Per-page primary key is positional: the entry's
/// position in the index (`feature_idx`) is what attrs/class/label sidecars
/// join against. Stride 33 is unaligned, so the decoder copies each field
/// through `from_le_bytes` rather than zero-copy casting to `&[FeatureEntry]`.
pub(crate) const FEATURE_INDEX_ENTRY_LEN: usize = 8 + 4 * 4 + 1 + 4 + 4;

/// One feature's index entry: user_id (data), approximate bbox, and pointer
/// into the coord area. Decoded lazily by [`FeatureIndexIter`]; coordinates
/// stay untouched until [`decode_one_geom`] is called for a chosen entry.
/// The substrate primary key is positional - see [`FeatureIndexIter`] for
/// `(feature_idx, entry)` pairs.
#[derive(Debug, Clone, Copy)]
pub struct FeatureIndexEntry {
    /// Source-supplied identifier; non-unique. See [`FeatureGeom::user_id`].
    pub user_id: u64,
    pub bbox: [f32; 4],
    pub geom_type: u8,
    /// offset into the coord area (the bytes after the index header).
    pub coord_offset: u32,
    pub coord_len: u32,
}

impl FeatureIndexEntry {
    /// Decode the geometry kind from the on-wire `geom_type` byte. Returns a
    /// typed [`GeomType`] so callers can branch without re-knowing the byte
    /// constants.
    pub fn geom_kind(&self) -> Result<GeomType, ArtifactError> {
        match self.geom_type {
            GT_POINT => Ok(GeomType::Point),
            GT_LINESTRING => Ok(GeomType::LineString),
            GT_POLYGON => Ok(GeomType::Polygon),
            GT_MULTIPOINT => Ok(GeomType::MultiPoint),
            GT_MULTILINESTRING => Ok(GeomType::MultiLineString),
            GT_MULTIPOLYGON => Ok(GeomType::MultiPolygon),
            _ => Err(ArtifactError::Malformed("bad geom_type")),
        }
    }
}

/// Lazy iterator over the geometry-payload index. Cheap: each step copies
/// one 33-byte index entry. Coordinates are not touched.
pub struct FeatureIndexIter<'a> {
    bytes: &'a [u8],
    coord_area_len: usize,
    count: usize,
    pos: usize,
}

impl<'a> FeatureIndexIter<'a> {
    /// Bytes of the coord area (the region after the fixed-stride index
    /// header). Useful for callers that decode geometries on the fly.
    #[must_use]
    pub fn coord_area(&self) -> &'a [u8] {
        &self.bytes[self.bytes.len() - self.coord_area_len..]
    }

    /// Number of index entries in the payload.
    #[must_use]
    pub fn len(&self) -> usize {
        self.count
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

impl Iterator for FeatureIndexIter<'_> {
    type Item = Result<FeatureIndexEntry, ArtifactError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.count {
            return None;
        }
        let off = 4 + self.pos * FEATURE_INDEX_ENTRY_LEN;
        self.pos += 1;
        let entry = (|| -> Result<FeatureIndexEntry, ArtifactError> {
            let user_id = u64::from_le_bytes(read_array::<8>(self.bytes, off)?);
            let bbox = [
                f32::from_le_bytes(read_array::<4>(self.bytes, off + 8)?),
                f32::from_le_bytes(read_array::<4>(self.bytes, off + 12)?),
                f32::from_le_bytes(read_array::<4>(self.bytes, off + 16)?),
                f32::from_le_bytes(read_array::<4>(self.bytes, off + 20)?),
            ];
            let geom_type = self.bytes[off + 24];
            let coord_offset = u32::from_le_bytes(read_array::<4>(self.bytes, off + 25)?);
            let coord_len = u32::from_le_bytes(read_array::<4>(self.bytes, off + 29)?);
            // bound-check the entry up front so callers can decode by entry
            // without re-validating each time.
            let end = (coord_offset as usize)
                .checked_add(coord_len as usize)
                .ok_or(ArtifactError::Truncated)?;
            if end > self.coord_area_len {
                return Err(ArtifactError::Truncated);
            }
            Ok(FeatureIndexEntry {
                user_id,
                bbox,
                geom_type,
                coord_offset,
                coord_len,
            })
        })();
        Some(entry)
    }
}

/// Build a [`FeatureIndexIter`] over a geometry-payload section.
pub fn iter_feature_index(bytes: &[u8]) -> Result<FeatureIndexIter<'_>, ArtifactError> {
    let (count, header_len) = parse_payload_header(bytes)?;
    let coord_area_len = bytes.len() - header_len;
    Ok(FeatureIndexIter {
        bytes,
        coord_area_len,
        count,
        pos: 0,
    })
}

pub(super) fn parse_payload_header(bytes: &[u8]) -> Result<(usize, usize), ArtifactError> {
    if bytes.len() < 4 {
        return Err(ArtifactError::Truncated);
    }
    let count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let header_len = 4usize
        .checked_add(
            count
                .checked_mul(FEATURE_INDEX_ENTRY_LEN)
                .ok_or(ArtifactError::Truncated)?,
        )
        .ok_or(ArtifactError::Truncated)?;
    if bytes.len() < header_len {
        return Err(ArtifactError::Truncated);
    }
    Ok((count, header_len))
}

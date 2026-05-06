//! geometry payload v1 codec. see FORMAT.md.
//!
//! ZERO-COPY CAVEAT: SPEC §9.3 promises that `geometry_index` can be mmap'd
//! and zero-copy cast to a typed slice. This is NOT possible with the current
//! 33-byte stride per `FEATURE_INDEX_ENTRY_LEN` (no field is naturally aligned
//! after the leading u64). The decoder copies each field via
//! `from_le_bytes`. SPEC §9.3 must be amended in a future format-bump pass.

use bytes::Bytes;

use crate::{
    ArtifactError,
    varint::{read_ivarint, read_uvarint, write_ivarint, write_uvarint},
};

pub type Coord = (f64, f64);

#[derive(Debug, Clone, PartialEq)]
pub enum GeomKind {
    Point(Coord),
    LineString(Vec<Coord>),
    Polygon(Vec<Vec<Coord>>),
    MultiPoint(Vec<Coord>),
    MultiLineString(Vec<Vec<Coord>>),
    MultiPolygon(Vec<Vec<Vec<Coord>>>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct FeatureGeom {
    pub id: u64,
    /// Per-feature bounding box stored as f32 per SPEC §9.3. At canonical-CRS
    /// magnitudes (~6e5 m for Danish UTM-32) this is ~0.05 m of precision,
    /// so the index bbox is APPROXIMATE: feature-level filtering must not
    /// rely on it for sub-meter discrimination - re-test against the decoded
    /// geometry when accuracy matters.
    pub bbox: [f32; 4],
    pub geom: GeomKind,
}

const GT_POINT: u8 = 1;
const GT_LINESTRING: u8 = 2;
const GT_POLYGON: u8 = 3;
const GT_MULTIPOINT: u8 = 4;
const GT_MULTILINESTRING: u8 = 5;
const GT_MULTIPOLYGON: u8 = 6;

// quantization is mm-precision fixed point. i64 holds ±9.2e18 mm = ±9.2e15 m
// of representable canonical-CRS extent; anything beyond is a coding bug or
// corrupt input and surfaces as ArtifactError::CoordOutOfRange.
#[inline]
fn quantize(c: f64) -> Result<i64, ArtifactError> {
    if !c.is_finite() {
        return Err(ArtifactError::CoordOutOfRange(c));
    }
    let scaled = (c * 1000.0).round();
    if scaled > i64::MAX as f64 || scaled < i64::MIN as f64 {
        return Err(ArtifactError::CoordOutOfRange(c));
    }
    Ok(scaled as i64)
}

#[inline]
fn dequantize(q: i64) -> f64 {
    (q as f64) / 1000.0
}

fn write_ring(out: &mut Vec<u8>, ring: &[Coord]) -> Result<(), ArtifactError> {
    write_uvarint(out, ring.len() as u64);
    if ring.is_empty() {
        return Ok(());
    }
    let (mut px, mut py) = (quantize(ring[0].0)?, quantize(ring[0].1)?);
    write_ivarint(out, px);
    write_ivarint(out, py);
    for &(x, y) in &ring[1..] {
        let (qx, qy) = (quantize(x)?, quantize(y)?);
        let dx = qx.checked_sub(px).ok_or(ArtifactError::CoordOutOfRange(x))?;
        let dy = qy.checked_sub(py).ok_or(ArtifactError::CoordOutOfRange(y))?;
        write_ivarint(out, dx);
        write_ivarint(out, dy);
        px = qx;
        py = qy;
    }
    Ok(())
}

fn read_uvarint_usize(buf: &[u8], pos: &mut usize) -> Result<usize, ArtifactError> {
    read_uvarint(buf, pos)?
        .try_into()
        .map_err(|_| ArtifactError::Malformed("count exceeds usize"))
}

fn read_ring(buf: &[u8], pos: &mut usize) -> Result<Vec<Coord>, ArtifactError> {
    let n = read_uvarint_usize(buf, pos)?;
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(n);
    let mut px = read_ivarint(buf, pos)?;
    let mut py = read_ivarint(buf, pos)?;
    out.push((dequantize(px), dequantize(py)));
    for _ in 1..n {
        px += read_ivarint(buf, pos)?;
        py += read_ivarint(buf, pos)?;
        out.push((dequantize(px), dequantize(py)));
    }
    Ok(out)
}

fn write_geom(out: &mut Vec<u8>, g: &GeomKind) -> Result<(), ArtifactError> {
    match g {
        GeomKind::Point((x, y)) => {
            write_ivarint(out, quantize(*x)?);
            write_ivarint(out, quantize(*y)?);
        }
        GeomKind::LineString(verts) => write_ring(out, verts)?,
        GeomKind::Polygon(rings) => {
            write_uvarint(out, rings.len() as u64);
            for r in rings {
                write_ring(out, r)?;
            }
        }
        GeomKind::MultiPoint(parts) => {
            write_uvarint(out, parts.len() as u64);
            for &(x, y) in parts {
                write_ivarint(out, quantize(x)?);
                write_ivarint(out, quantize(y)?);
            }
        }
        GeomKind::MultiLineString(parts) => {
            write_uvarint(out, parts.len() as u64);
            for p in parts {
                write_ring(out, p)?;
            }
        }
        GeomKind::MultiPolygon(parts) => {
            write_uvarint(out, parts.len() as u64);
            for poly in parts {
                write_uvarint(out, poly.len() as u64);
                for r in poly {
                    write_ring(out, r)?;
                }
            }
        }
    }
    Ok(())
}

fn read_geom(geom_type: u8, buf: &[u8], pos: &mut usize) -> Result<GeomKind, ArtifactError> {
    Ok(match geom_type {
        GT_POINT => {
            let x = read_ivarint(buf, pos)?;
            let y = read_ivarint(buf, pos)?;
            GeomKind::Point((dequantize(x), dequantize(y)))
        }
        GT_LINESTRING => GeomKind::LineString(read_ring(buf, pos)?),
        GT_POLYGON => {
            let n = read_uvarint_usize(buf, pos)?;
            let mut rings = Vec::with_capacity(n);
            for _ in 0..n {
                rings.push(read_ring(buf, pos)?);
            }
            GeomKind::Polygon(rings)
        }
        GT_MULTIPOINT => {
            let n = read_uvarint_usize(buf, pos)?;
            let mut pts = Vec::with_capacity(n);
            for _ in 0..n {
                let x = read_ivarint(buf, pos)?;
                let y = read_ivarint(buf, pos)?;
                pts.push((dequantize(x), dequantize(y)));
            }
            GeomKind::MultiPoint(pts)
        }
        GT_MULTILINESTRING => {
            let n = read_uvarint_usize(buf, pos)?;
            let mut parts = Vec::with_capacity(n);
            for _ in 0..n {
                parts.push(read_ring(buf, pos)?);
            }
            GeomKind::MultiLineString(parts)
        }
        GT_MULTIPOLYGON => {
            let n = read_uvarint_usize(buf, pos)?;
            let mut polys = Vec::with_capacity(n);
            for _ in 0..n {
                let m = read_uvarint_usize(buf, pos)?;
                let mut rings = Vec::with_capacity(m);
                for _ in 0..m {
                    rings.push(read_ring(buf, pos)?);
                }
                polys.push(rings);
            }
            GeomKind::MultiPolygon(polys)
        }
        _ => return Err(ArtifactError::Malformed("bad geom_type")),
    })
}

fn geom_type_byte(g: &GeomKind) -> u8 {
    match g {
        GeomKind::Point(_) => GT_POINT,
        GeomKind::LineString(_) => GT_LINESTRING,
        GeomKind::Polygon(_) => GT_POLYGON,
        GeomKind::MultiPoint(_) => GT_MULTIPOINT,
        GeomKind::MultiLineString(_) => GT_MULTILINESTRING,
        GeomKind::MultiPolygon(_) => GT_MULTIPOLYGON,
    }
}

/// Length in bytes of one feature index entry.
///
/// Layout: u64 id, [f32; 4] bbox, u8 geom_type, u32 coord_offset, u32 coord_len.
/// Stride 33 is unaligned: the index is decoded by copying each field through
/// `from_le_bytes`. Zero-copy cast to `&[FeatureEntry]` is NOT supported with
/// the current stride. SPEC §9.3 must be amended in a future format-bump pass.
const FEATURE_INDEX_ENTRY_LEN: usize = 8 + 4 * 4 + 1 + 4 + 4;

/// encode features into the geometry-payload section bytes.
///
/// Features must be sorted by `id` ascending (required for determinism and
/// for the index to support binary search). Violation returns
/// `ArtifactError::UnsortedFeatures` in release; debug builds also assert.
pub fn encode_geometry_payload(features: &[FeatureGeom]) -> Result<Bytes, ArtifactError> {
    if !features.windows(2).all(|w| w[0].id < w[1].id) {
        return Err(ArtifactError::UnsortedFeatures);
    }
    // pack coord blocks first to learn their offsets, then write index + blocks
    let mut coord_blocks: Vec<Vec<u8>> = Vec::with_capacity(features.len());
    let mut total_coord_bytes: usize = 0;
    for f in features {
        let mut buf = Vec::new();
        write_geom(&mut buf, &f.geom)?;
        total_coord_bytes = total_coord_bytes
            .checked_add(buf.len())
            .ok_or(ArtifactError::Malformed("geometry payload too large"))?;
        coord_blocks.push(buf);
    }

    let header_len = 4usize
        .checked_add(
            features
                .len()
                .checked_mul(FEATURE_INDEX_ENTRY_LEN)
                .ok_or(ArtifactError::Malformed("geometry payload too large"))?,
        )
        .ok_or(ArtifactError::Malformed("geometry payload too large"))?;
    let total_len = header_len
        .checked_add(total_coord_bytes)
        .ok_or(ArtifactError::Malformed("geometry payload too large"))?;
    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(
        &(u32::try_from(features.len()).map_err(|_| ArtifactError::Malformed("too many features"))?).to_le_bytes(),
    );

    let mut running_offset: u32 = 0;
    for (f, block) in features.iter().zip(&coord_blocks) {
        out.extend_from_slice(&f.id.to_le_bytes());
        for v in &f.bbox {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.push(geom_type_byte(&f.geom));
        out.extend_from_slice(&running_offset.to_le_bytes());
        let len = u32::try_from(block.len()).map_err(|_| ArtifactError::Malformed("geometry section too large"))?;
        out.extend_from_slice(&len.to_le_bytes());
        running_offset = running_offset
            .checked_add(len)
            .ok_or(ArtifactError::Malformed("geometry payload too large"))?;
    }
    for block in &coord_blocks {
        out.extend_from_slice(block);
    }
    Ok(Bytes::from(out))
}

/// Read a fixed-size little-endian array out of `slice` at `offset`.
///
/// Returns `Truncated` if the read would go out of bounds. Caller may have
/// already validated the entire region's bound; this helper still checks
/// defensively because the cost is negligible vs the noise of inlined slicing.
#[inline]
fn read_array<const N: usize>(slice: &[u8], offset: usize) -> Result<[u8; N], ArtifactError> {
    slice
        .get(offset..offset.checked_add(N).ok_or(ArtifactError::Truncated)?)
        .ok_or(ArtifactError::Truncated)?
        .try_into()
        .map_err(|_| ArtifactError::Truncated)
}

pub fn decode_geometry_payload(bytes: &[u8]) -> Result<Vec<FeatureGeom>, ArtifactError> {
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
    let coord_area = &bytes[header_len..];

    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = 4 + i * FEATURE_INDEX_ENTRY_LEN;
        let id = u64::from_le_bytes(read_array::<8>(bytes, off)?);
        let bbox = [
            f32::from_le_bytes(read_array::<4>(bytes, off + 8)?),
            f32::from_le_bytes(read_array::<4>(bytes, off + 12)?),
            f32::from_le_bytes(read_array::<4>(bytes, off + 16)?),
            f32::from_le_bytes(read_array::<4>(bytes, off + 20)?),
        ];
        let geom_type = bytes[off + 24];
        let coff = u32::from_le_bytes(read_array::<4>(bytes, off + 25)?) as usize;
        let clen = u32::from_le_bytes(read_array::<4>(bytes, off + 29)?) as usize;
        let coord_end = coff.checked_add(clen).ok_or(ArtifactError::Truncated)?;
        if coord_end > coord_area.len() {
            return Err(ArtifactError::Truncated);
        }
        let mut pos = coff;
        let geom = read_geom(geom_type, coord_area, &mut pos)?;
        if pos != coord_end {
            return Err(ArtifactError::Malformed("coord block length mismatch"));
        }
        out.push(FeatureGeom { id, bbox, geom });
    }
    Ok(out)
}

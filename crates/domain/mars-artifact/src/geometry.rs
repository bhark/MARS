//! geometry payload v1 codec. see FORMAT.md.

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
    pub bbox: [f32; 4],
    pub geom: GeomKind,
}

const GT_POINT: u8 = 1;
const GT_LINESTRING: u8 = 2;
const GT_POLYGON: u8 = 3;
const GT_MULTIPOINT: u8 = 4;
const GT_MULTILINESTRING: u8 = 5;
const GT_MULTIPOLYGON: u8 = 6;

#[inline]
fn quantize(c: f64) -> i64 {
    (c * 1000.0).round() as i64
}

#[inline]
fn dequantize(q: i64) -> f64 {
    (q as f64) / 1000.0
}

fn write_ring(out: &mut Vec<u8>, ring: &[Coord]) {
    write_uvarint(out, ring.len() as u64);
    if ring.is_empty() {
        return;
    }
    let (mut px, mut py) = (quantize(ring[0].0), quantize(ring[0].1));
    write_ivarint(out, px);
    write_ivarint(out, py);
    for &(x, y) in &ring[1..] {
        let (qx, qy) = (quantize(x), quantize(y));
        write_ivarint(out, qx - px);
        write_ivarint(out, qy - py);
        px = qx;
        py = qy;
    }
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

fn write_geom(out: &mut Vec<u8>, g: &GeomKind) {
    match g {
        GeomKind::Point((x, y)) => {
            write_ivarint(out, quantize(*x));
            write_ivarint(out, quantize(*y));
        }
        GeomKind::LineString(verts) => write_ring(out, verts),
        GeomKind::Polygon(rings) => {
            write_uvarint(out, rings.len() as u64);
            for r in rings {
                write_ring(out, r);
            }
        }
        GeomKind::MultiPoint(parts) => {
            write_uvarint(out, parts.len() as u64);
            for &(x, y) in parts {
                write_ivarint(out, quantize(x));
                write_ivarint(out, quantize(y));
            }
        }
        GeomKind::MultiLineString(parts) => {
            write_uvarint(out, parts.len() as u64);
            for p in parts {
                write_ring(out, p);
            }
        }
        GeomKind::MultiPolygon(parts) => {
            write_uvarint(out, parts.len() as u64);
            for poly in parts {
                write_uvarint(out, poly.len() as u64);
                for r in poly {
                    write_ring(out, r);
                }
            }
        }
    }
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

const FEATURE_INDEX_ENTRY_LEN: usize = 8 + 4 * 4 + 1 + 4 + 4;

/// encode features into the geometry-payload section bytes.
/// requires features sorted by id ascending (caller's responsibility for determinism).
pub fn encode_geometry_payload(features: &[FeatureGeom]) -> Result<Bytes, ArtifactError> {
    // pack coord blocks first to learn their offsets, then write index + blocks
    let mut coord_blocks: Vec<Vec<u8>> = Vec::with_capacity(features.len());
    let mut total_coord_bytes: usize = 0;
    for f in features {
        let mut buf = Vec::new();
        write_geom(&mut buf, &f.geom);
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

pub fn decode_geometry_payload(bytes: &[u8]) -> Result<Vec<FeatureGeom>, ArtifactError> {
    if bytes.len() < 4 {
        return Err(ArtifactError::Truncated);
    }
    let count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let header_len = 4 + count * FEATURE_INDEX_ENTRY_LEN;
    if bytes.len() < header_len {
        return Err(ArtifactError::Truncated);
    }
    let coord_base = header_len;
    let coord_area = &bytes[coord_base..];

    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = 4 + i * FEATURE_INDEX_ENTRY_LEN;
        let id = u64::from_le_bytes(bytes[off..off + 8].try_into().map_err(|_| ArtifactError::Truncated)?);
        let bbox = [
            f32::from_le_bytes(
                bytes[off + 8..off + 12]
                    .try_into()
                    .map_err(|_| ArtifactError::Truncated)?,
            ),
            f32::from_le_bytes(
                bytes[off + 12..off + 16]
                    .try_into()
                    .map_err(|_| ArtifactError::Truncated)?,
            ),
            f32::from_le_bytes(
                bytes[off + 16..off + 20]
                    .try_into()
                    .map_err(|_| ArtifactError::Truncated)?,
            ),
            f32::from_le_bytes(
                bytes[off + 20..off + 24]
                    .try_into()
                    .map_err(|_| ArtifactError::Truncated)?,
            ),
        ];
        let geom_type = bytes[off + 24];
        let coff = u32::from_le_bytes(
            bytes[off + 25..off + 29]
                .try_into()
                .map_err(|_| ArtifactError::Truncated)?,
        ) as usize;
        let clen = u32::from_le_bytes(
            bytes[off + 29..off + 33]
                .try_into()
                .map_err(|_| ArtifactError::Truncated)?,
        ) as usize;
        if coff.checked_add(clen).is_none_or(|end| end > coord_area.len()) {
            return Err(ArtifactError::Truncated);
        }
        let mut pos = coff;
        let geom = read_geom(geom_type, coord_area, &mut pos)?;
        if pos != coff + clen {
            return Err(ArtifactError::Malformed("coord block length mismatch"));
        }
        out.push(FeatureGeom { id, bbox, geom });
    }
    Ok(out)
}

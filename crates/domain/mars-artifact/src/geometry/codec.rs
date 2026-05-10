use crate::{
    ArtifactError,
    varint::{read_ivarint, read_uvarint, write_ivarint, write_uvarint},
};

use super::{Coord, GeomKind, GT_MULTIPOINT, GT_MULTIPOLYGON, GT_MULTILINESTRING, GT_POINT, GT_POLYGON, GT_LINESTRING, MAX_GEOM_COORDS, MAX_GEOM_PARTS};

// quantization is mm-precision fixed point. i64 holds ±9.2e18 mm = ±9.2e15 m
// of representable canonical-CRS extent; anything beyond is a coding bug or
// corrupt input and surfaces as ArtifactError::CoordOutOfRange.
#[inline]
pub(super) fn quantize(c: f64) -> Result<i64, ArtifactError> {
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
pub(super) fn dequantize(q: i64) -> f64 {
    (q as f64) * 0.001
}

pub(super) fn write_ring(out: &mut Vec<u8>, ring: &[Coord]) -> Result<(), ArtifactError> {
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

pub(super) fn read_uvarint_usize(buf: &[u8], pos: &mut usize) -> Result<usize, ArtifactError> {
    read_uvarint(buf, pos)?
        .try_into()
        .map_err(|_| ArtifactError::Malformed("count exceeds usize"))
}

pub(super) fn read_ring(buf: &[u8], pos: &mut usize) -> Result<Vec<Coord>, ArtifactError> {
    let n = read_uvarint_usize(buf, pos)?;
    if n > MAX_GEOM_COORDS {
        return Err(ArtifactError::Malformed("ring coordinate count exceeds limit"));
    }
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(n);
    let mut px = read_ivarint(buf, pos)?;
    let mut py = read_ivarint(buf, pos)?;
    out.push((dequantize(px), dequantize(py)));
    for _ in 1..n {
        let dx = read_ivarint(buf, pos)?;
        let dy = read_ivarint(buf, pos)?;
        px = px
            .checked_add(dx)
            .ok_or(ArtifactError::Malformed("coord delta overflow"))?;
        py = py
            .checked_add(dy)
            .ok_or(ArtifactError::Malformed("coord delta overflow"))?;
        out.push((dequantize(px), dequantize(py)));
    }
    Ok(out)
}

pub(super) fn write_geom(out: &mut Vec<u8>, g: &GeomKind) -> Result<(), ArtifactError> {
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

pub(super) fn read_geom(geom_type: u8, buf: &[u8], pos: &mut usize) -> Result<GeomKind, ArtifactError> {
    Ok(match geom_type {
        GT_POINT => {
            let x = read_ivarint(buf, pos)?;
            let y = read_ivarint(buf, pos)?;
            GeomKind::Point((dequantize(x), dequantize(y)))
        }
        GT_LINESTRING => GeomKind::LineString(read_ring(buf, pos)?),
        GT_POLYGON => {
            let n = read_uvarint_usize(buf, pos)?;
            if n > MAX_GEOM_PARTS {
                return Err(ArtifactError::Malformed("polygon ring count exceeds limit"));
            }
            let mut rings = Vec::with_capacity(n);
            for _ in 0..n {
                rings.push(read_ring(buf, pos)?);
            }
            GeomKind::Polygon(rings)
        }
        GT_MULTIPOINT => {
            let n = read_uvarint_usize(buf, pos)?;
            if n > MAX_GEOM_COORDS {
                return Err(ArtifactError::Malformed("multipoint count exceeds limit"));
            }
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
            if n > MAX_GEOM_PARTS {
                return Err(ArtifactError::Malformed("multilinestring part count exceeds limit"));
            }
            let mut parts = Vec::with_capacity(n);
            for _ in 0..n {
                parts.push(read_ring(buf, pos)?);
            }
            GeomKind::MultiLineString(parts)
        }
        GT_MULTIPOLYGON => {
            let n = read_uvarint_usize(buf, pos)?;
            if n > MAX_GEOM_PARTS {
                return Err(ArtifactError::Malformed("multipolygon count exceeds limit"));
            }
            let mut polys = Vec::with_capacity(n);
            for _ in 0..n {
                let m = read_uvarint_usize(buf, pos)?;
                if m > MAX_GEOM_PARTS {
                    return Err(ArtifactError::Malformed("multipolygon ring count exceeds limit"));
                }
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

pub(super) fn geom_type_byte(g: &GeomKind) -> u8 {
    match g {
        GeomKind::Point(_) => GT_POINT,
        GeomKind::LineString(_) => GT_LINESTRING,
        GeomKind::Polygon(_) => GT_POLYGON,
        GeomKind::MultiPoint(_) => GT_MULTIPOINT,
        GeomKind::MultiLineString(_) => GT_MULTILINESTRING,
        GeomKind::MultiPolygon(_) => GT_MULTIPOLYGON,
    }
}

/// Read a fixed-size little-endian array out of `slice` at `offset`.
///
/// Returns `Truncated` if the read would go out of bounds. Caller may have
/// already validated the entire region's bound; this helper still checks
/// defensively because the cost is negligible vs the noise of inlined slicing.
#[inline]
pub(super) fn read_array<const N: usize>(slice: &[u8], offset: usize) -> Result<[u8; N], ArtifactError> {
    slice
        .get(offset..offset.checked_add(N).ok_or(ArtifactError::Truncated)?)
        .ok_or(ArtifactError::Truncated)?
        .try_into()
        .map_err(|_| ArtifactError::Truncated)
}

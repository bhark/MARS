//! Shared scratch-disk codec for [`KeyedRow`].
//!
//! Both [`crate::spill`] and [`crate::external_sort`] write `KeyedRow`
//! batches to per-process temporary files. The framing differs (spill:
//! magic + version + kind-tagged records; external_sort: u64 count +
//! length-prefixed records), but the row body shape is identical, so it
//! is centralised here.
//!
//! Format (per row body): `key u64 LE | user_id u64 LE | bbox 4×f32 LE |
//! geom (1-byte tag + payload) | attr_count u32 LE | attrs (u32 name_len
//! LE, name bytes, 1-byte attr tag, payload)* | geom_bytes_estimate u64
//! LE | row_fingerprint u64 LE`. Process-local, ephemeral, no checksum,
//! no cross-version stability.

use std::sync::Arc;

use mars_artifact::{Coord, FeatureGeom, GeomKind};
use mars_source::AttrValue;
use mars_types::HilbertKey;

use crate::CompilerError;
use crate::render::KeyedRow;

// geom variant tags
const GT_POINT: u8 = 1;
const GT_LINESTRING: u8 = 2;
const GT_POLYGON: u8 = 3;
const GT_MULTIPOINT: u8 = 4;
const GT_MULTILINESTRING: u8 = 5;
const GT_MULTIPOLYGON: u8 = 6;

// attr variant tags
const AT_NULL: u8 = 0;
const AT_BOOL: u8 = 1;
const AT_INT: u8 = 2;
const AT_FLOAT: u8 = 3;
const AT_STRING: u8 = 4;

/// Saturating `usize -> u32` cast for length prefixes. Lengths above 4 GiB
/// would have failed elsewhere long before reaching this codec; saturating
/// keeps the encoder infallible without smuggling in a panic path.
#[inline]
pub(crate) fn u32_try(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Reader abstraction so the same decoder serves spill (`BufReader<File>`
/// with an external byte counter) and external_sort (an in-buffer slice
/// cursor). Each impl maps its transport-level error onto [`CompilerError`].
pub(crate) trait ScratchReader {
    fn u8(&mut self) -> Result<u8, CompilerError>;
    fn u32(&mut self) -> Result<u32, CompilerError>;
    fn u64(&mut self) -> Result<u64, CompilerError>;
    fn i64(&mut self) -> Result<i64, CompilerError>;
    fn f32(&mut self) -> Result<f32, CompilerError>;
    fn f64(&mut self) -> Result<f64, CompilerError>;
    fn take(&mut self, n: usize) -> Result<&[u8], CompilerError>;
}

pub(crate) fn encode_keyed_row_body(buf: &mut Vec<u8>, r: &KeyedRow) {
    buf.extend_from_slice(&r.key.get().to_le_bytes());
    buf.extend_from_slice(&r.feature.user_id.to_le_bytes());
    for c in r.feature.bbox {
        buf.extend_from_slice(&c.to_le_bytes());
    }
    encode_geom(buf, &r.feature.geom);
    let attrs = r.attrs.as_slice();
    buf.extend_from_slice(&u32_try(attrs.len()).to_le_bytes());
    for (name, value) in attrs {
        let nb = name.as_bytes();
        buf.extend_from_slice(&u32_try(nb.len()).to_le_bytes());
        buf.extend_from_slice(nb);
        encode_attr(buf, value);
    }
    buf.extend_from_slice(&r.geom_bytes_estimate.to_le_bytes());
    buf.extend_from_slice(&r.row_fingerprint.to_le_bytes());
}

pub(crate) fn decode_keyed_row_body<R: ScratchReader>(r: &mut R) -> Result<KeyedRow, CompilerError> {
    let key = HilbertKey::new(r.u64()?);
    let user_id = r.u64()?;
    let mut bbox = [0f32; 4];
    for c in &mut bbox {
        *c = r.f32()?;
    }
    let geom = decode_geom(r)?;
    let attr_count = r.u32()? as usize;
    let mut attrs: Vec<(String, AttrValue)> = Vec::with_capacity(attr_count);
    for _ in 0..attr_count {
        let nlen = r.u32()? as usize;
        let nb = r.take(nlen)?.to_vec();
        let name = String::from_utf8(nb).map_err(|_| CompilerError::InvariantViolation {
            what: "scratch_codec: bad utf8 in attr name",
        })?;
        let value = decode_attr(r)?;
        attrs.push((name, value));
    }
    let geom_bytes_estimate = r.u64()?;
    let row_fingerprint = r.u64()?;
    Ok(KeyedRow {
        feature: FeatureGeom { user_id, bbox, geom },
        attrs: Arc::new(attrs),
        geom_bytes_estimate,
        key,
        row_fingerprint,
    })
}

fn encode_geom(buf: &mut Vec<u8>, g: &GeomKind) {
    match g {
        GeomKind::Point((x, y)) => {
            buf.push(GT_POINT);
            buf.extend_from_slice(&x.to_le_bytes());
            buf.extend_from_slice(&y.to_le_bytes());
        }
        GeomKind::LineString(coords) => {
            buf.push(GT_LINESTRING);
            encode_coords(buf, coords);
        }
        GeomKind::Polygon(rings) => {
            buf.push(GT_POLYGON);
            buf.extend_from_slice(&u32_try(rings.len()).to_le_bytes());
            for ring in rings {
                encode_coords(buf, ring);
            }
        }
        GeomKind::MultiPoint(points) => {
            buf.push(GT_MULTIPOINT);
            encode_coords(buf, points);
        }
        GeomKind::MultiLineString(parts) => {
            buf.push(GT_MULTILINESTRING);
            buf.extend_from_slice(&u32_try(parts.len()).to_le_bytes());
            for p in parts {
                encode_coords(buf, p);
            }
        }
        GeomKind::MultiPolygon(parts) => {
            buf.push(GT_MULTIPOLYGON);
            buf.extend_from_slice(&u32_try(parts.len()).to_le_bytes());
            for poly in parts {
                buf.extend_from_slice(&u32_try(poly.len()).to_le_bytes());
                for ring in poly {
                    encode_coords(buf, ring);
                }
            }
        }
    }
}

fn decode_geom<R: ScratchReader>(r: &mut R) -> Result<GeomKind, CompilerError> {
    Ok(match r.u8()? {
        GT_POINT => {
            let x = r.f64()?;
            let y = r.f64()?;
            GeomKind::Point((x, y))
        }
        GT_LINESTRING => GeomKind::LineString(decode_coords(r)?),
        GT_POLYGON => {
            let rings = r.u32()? as usize;
            let mut out = Vec::with_capacity(rings);
            for _ in 0..rings {
                out.push(decode_coords(r)?);
            }
            GeomKind::Polygon(out)
        }
        GT_MULTIPOINT => GeomKind::MultiPoint(decode_coords(r)?),
        GT_MULTILINESTRING => {
            let parts = r.u32()? as usize;
            let mut out = Vec::with_capacity(parts);
            for _ in 0..parts {
                out.push(decode_coords(r)?);
            }
            GeomKind::MultiLineString(out)
        }
        GT_MULTIPOLYGON => {
            let parts = r.u32()? as usize;
            let mut out = Vec::with_capacity(parts);
            for _ in 0..parts {
                let rings = r.u32()? as usize;
                let mut poly = Vec::with_capacity(rings);
                for _ in 0..rings {
                    poly.push(decode_coords(r)?);
                }
                out.push(poly);
            }
            GeomKind::MultiPolygon(out)
        }
        _ => {
            return Err(CompilerError::InvariantViolation {
                what: "scratch_codec: bad geom tag",
            });
        }
    })
}

fn encode_coords(buf: &mut Vec<u8>, c: &[Coord]) {
    buf.extend_from_slice(&u32_try(c.len()).to_le_bytes());
    for (x, y) in c {
        buf.extend_from_slice(&x.to_le_bytes());
        buf.extend_from_slice(&y.to_le_bytes());
    }
}

fn decode_coords<R: ScratchReader>(r: &mut R) -> Result<Vec<Coord>, CompilerError> {
    let n = r.u32()? as usize;
    let mut out: Vec<Coord> = Vec::with_capacity(n);
    for _ in 0..n {
        let x = r.f64()?;
        let y = r.f64()?;
        out.push((x, y));
    }
    Ok(out)
}

fn encode_attr(buf: &mut Vec<u8>, v: &AttrValue) {
    match v {
        AttrValue::Null => buf.push(AT_NULL),
        AttrValue::Bool(b) => {
            buf.push(AT_BOOL);
            buf.push(u8::from(*b));
        }
        AttrValue::Int(i) => {
            buf.push(AT_INT);
            buf.extend_from_slice(&i.to_le_bytes());
        }
        AttrValue::Float(f) => {
            buf.push(AT_FLOAT);
            buf.extend_from_slice(&f.to_le_bytes());
        }
        AttrValue::String(s) => {
            buf.push(AT_STRING);
            let sb = s.as_bytes();
            buf.extend_from_slice(&u32_try(sb.len()).to_le_bytes());
            buf.extend_from_slice(sb);
        }
    }
}

fn decode_attr<R: ScratchReader>(r: &mut R) -> Result<AttrValue, CompilerError> {
    Ok(match r.u8()? {
        AT_NULL => AttrValue::Null,
        AT_BOOL => AttrValue::Bool(r.u8()? != 0),
        AT_INT => AttrValue::Int(r.i64()?),
        AT_FLOAT => AttrValue::Float(r.f64()?),
        AT_STRING => {
            let n = r.u32()? as usize;
            let sb = r.take(n)?.to_vec();
            let s = String::from_utf8(sb).map_err(|_| CompilerError::InvariantViolation {
                what: "scratch_codec: bad utf8 in attr string",
            })?;
            AttrValue::String(s)
        }
        _ => {
            return Err(CompilerError::InvariantViolation {
                what: "scratch_codec: bad attr tag",
            });
        }
    })
}

#[cfg(test)]
mod tests;

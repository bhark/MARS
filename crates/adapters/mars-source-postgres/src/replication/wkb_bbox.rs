//! Bbox-only WKB / EWKB extractor.
//!
//! Allocates nothing: walks the byte stream, accumulates min/max x and y, and
//! returns. Supports the six geometry types we care about (point, linestring,
//! polygon, multipoint, multilinestring, multipolygon) and skips Z/M
//! ordinates. Geometry collections, curves, and TIN are rejected.

use mars_types::Bbox;

#[derive(Debug, thiserror::Error)]
pub(crate) enum WkbBboxError {
    #[error("truncated WKB")]
    Truncated,
    #[error("invalid byte-order flag: {0}")]
    BadEndian(u8),
    #[error("unsupported geometry type code: {0}")]
    UnsupportedType(u32),
    #[error("empty geometry")]
    Empty,
    #[error("WKB nesting too deep")]
    TooDeep,
}

// well-formed WKB nests Multi -> {Point,LineString,Polygon}, depth <= 2.
// cap generously while still bounding the call stack against malformed
// inputs that try to recurse arbitrarily.
const MAX_WKB_DEPTH: u32 = 8;

const WKB_POINT: u32 = 1;
const WKB_LINESTRING: u32 = 2;
const WKB_POLYGON: u32 = 3;
const WKB_MULTIPOINT: u32 = 4;
const WKB_MULTILINESTRING: u32 = 5;
const WKB_MULTIPOLYGON: u32 = 6;
const EWKB_SRID_FLAG: u32 = 0x2000_0000;
const EWKB_Z_FLAG: u32 = 0x8000_0000;
const EWKB_M_FLAG: u32 = 0x4000_0000;

/// Extract the bbox of an arbitrary supported WKB / EWKB geometry. Returns
/// `Empty` for geometries that contain no coordinates (zero-ring polygons,
/// empty multi-* containers).
pub(crate) fn bbox_of(wkb: &[u8]) -> Result<Bbox, WkbBboxError> {
    let mut acc = BboxAcc::new();
    let mut cur = Cursor::new(wkb);
    walk(&mut cur, &mut acc, 0)?;
    acc.finish()
}

struct BboxAcc {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    seen: bool,
}

impl BboxAcc {
    fn new() -> Self {
        Self {
            min_x: f64::INFINITY,
            min_y: f64::INFINITY,
            max_x: f64::NEG_INFINITY,
            max_y: f64::NEG_INFINITY,
            seen: false,
        }
    }
    fn add(&mut self, x: f64, y: f64) {
        if x.is_finite() && y.is_finite() {
            self.min_x = self.min_x.min(x);
            self.min_y = self.min_y.min(y);
            self.max_x = self.max_x.max(x);
            self.max_y = self.max_y.max(y);
            self.seen = true;
        }
    }
    fn finish(self) -> Result<Bbox, WkbBboxError> {
        if !self.seen {
            return Err(WkbBboxError::Empty);
        }
        Ok(Bbox::new(self.min_x, self.min_y, self.max_x, self.max_y))
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], WkbBboxError> {
        if self.pos + n > self.buf.len() {
            return Err(WkbBboxError::Truncated);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, WkbBboxError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self, le: bool) -> Result<u32, WkbBboxError> {
        let b = self.take(4)?;
        let arr: [u8; 4] = [b[0], b[1], b[2], b[3]];
        Ok(if le {
            u32::from_le_bytes(arr)
        } else {
            u32::from_be_bytes(arr)
        })
    }
    fn f64(&mut self, le: bool) -> Result<f64, WkbBboxError> {
        let b = self.take(8)?;
        let arr: [u8; 8] = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
        Ok(if le {
            f64::from_le_bytes(arr)
        } else {
            f64::from_be_bytes(arr)
        })
    }
}

fn walk(c: &mut Cursor<'_>, acc: &mut BboxAcc, depth: u32) -> Result<(), WkbBboxError> {
    if depth > MAX_WKB_DEPTH {
        return Err(WkbBboxError::TooDeep);
    }
    let endian = c.u8()?;
    let le = match endian {
        1 => true,
        0 => false,
        other => return Err(WkbBboxError::BadEndian(other)),
    };
    let raw = c.u32(le)?;
    let has_z = raw & EWKB_Z_FLAG != 0;
    let has_m = raw & EWKB_M_FLAG != 0;
    let has_srid = raw & EWKB_SRID_FLAG != 0;
    if has_srid {
        let _ = c.u32(le)?;
    }
    let gtype = raw & 0x0000_00FF;
    match gtype {
        WKB_POINT => {
            let x = c.f64(le)?;
            let y = c.f64(le)?;
            skip_zm(c, le, has_z, has_m)?;
            acc.add(x, y);
        }
        WKB_LINESTRING => walk_points(c, le, has_z, has_m, acc)?,
        WKB_POLYGON => {
            let nrings = c.u32(le)? as usize;
            for _ in 0..nrings {
                walk_points(c, le, has_z, has_m, acc)?;
            }
        }
        WKB_MULTIPOINT | WKB_MULTILINESTRING | WKB_MULTIPOLYGON => {
            let n = c.u32(le)? as usize;
            for _ in 0..n {
                walk(c, acc, depth + 1)?;
            }
        }
        other => return Err(WkbBboxError::UnsupportedType(other)),
    }
    Ok(())
}

fn walk_points(c: &mut Cursor<'_>, le: bool, has_z: bool, has_m: bool, acc: &mut BboxAcc) -> Result<(), WkbBboxError> {
    let n = c.u32(le)? as usize;
    for _ in 0..n {
        let x = c.f64(le)?;
        let y = c.f64(le)?;
        skip_zm(c, le, has_z, has_m)?;
        acc.add(x, y);
    }
    Ok(())
}

fn skip_zm(c: &mut Cursor<'_>, le: bool, z: bool, m: bool) -> Result<(), WkbBboxError> {
    if z {
        let _ = c.f64(le)?;
    }
    if m {
        let _ = c.f64(le)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn point_le(x: f64, y: f64) -> Vec<u8> {
        let mut v = vec![1u8];
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
        v
    }

    fn linestring_le(pts: &[(f64, f64)]) -> Vec<u8> {
        let mut v = vec![1u8];
        v.extend_from_slice(&2u32.to_le_bytes());
        v.extend_from_slice(&(pts.len() as u32).to_le_bytes());
        for (x, y) in pts {
            v.extend_from_slice(&x.to_le_bytes());
            v.extend_from_slice(&y.to_le_bytes());
        }
        v
    }

    fn polygon_le(rings: &[&[(f64, f64)]]) -> Vec<u8> {
        let mut v = vec![1u8];
        v.extend_from_slice(&3u32.to_le_bytes());
        v.extend_from_slice(&(rings.len() as u32).to_le_bytes());
        for ring in rings {
            v.extend_from_slice(&(ring.len() as u32).to_le_bytes());
            for (x, y) in *ring {
                v.extend_from_slice(&x.to_le_bytes());
                v.extend_from_slice(&y.to_le_bytes());
            }
        }
        v
    }

    fn ewkb_point_le(srid: u32, x: f64, y: f64) -> Vec<u8> {
        let mut v = vec![1u8];
        v.extend_from_slice(&(1u32 | EWKB_SRID_FLAG).to_le_bytes());
        v.extend_from_slice(&srid.to_le_bytes());
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
        v
    }

    #[test]
    fn point_bbox() {
        let bb = bbox_of(&point_le(1.5, 2.5)).unwrap();
        assert_eq!((bb.min_x, bb.min_y, bb.max_x, bb.max_y), (1.5, 2.5, 1.5, 2.5));
    }

    #[test]
    fn ewkb_point_bbox() {
        let bb = bbox_of(&ewkb_point_le(25832, -1.0, 0.0)).unwrap();
        assert_eq!((bb.min_x, bb.min_y), (-1.0, 0.0));
    }

    #[test]
    fn linestring_bbox() {
        let bb = bbox_of(&linestring_le(&[(0.0, 0.0), (10.0, -5.0), (-3.0, 7.0)])).unwrap();
        assert_eq!((bb.min_x, bb.min_y, bb.max_x, bb.max_y), (-3.0, -5.0, 10.0, 7.0));
    }

    #[test]
    fn polygon_bbox() {
        let bb = bbox_of(&polygon_le(&[&[
            (0.0, 0.0),
            (5.0, 0.0),
            (5.0, 5.0),
            (0.0, 5.0),
            (0.0, 0.0),
        ]]))
        .unwrap();
        assert_eq!((bb.min_x, bb.min_y, bb.max_x, bb.max_y), (0.0, 0.0, 5.0, 5.0));
    }

    #[test]
    fn multipolygon_bbox() {
        let mut v = vec![1u8];
        v.extend_from_slice(&6u32.to_le_bytes());
        v.extend_from_slice(&2u32.to_le_bytes());
        v.extend_from_slice(&polygon_le(&[&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]]));
        v.extend_from_slice(&polygon_le(&[&[
            (10.0, 10.0),
            (12.0, 10.0),
            (12.0, 12.0),
            (10.0, 10.0),
        ]]));
        let bb = bbox_of(&v).unwrap();
        assert_eq!((bb.min_x, bb.min_y, bb.max_x, bb.max_y), (0.0, 0.0, 12.0, 12.0));
    }

    #[test]
    fn empty_polygon_errors() {
        let bb = bbox_of(&polygon_le(&[]));
        assert!(matches!(bb, Err(WkbBboxError::Empty)));
    }

    #[test]
    fn unsupported_type_rejected() {
        let mut v = vec![1u8];
        v.extend_from_slice(&7u32.to_le_bytes()); // GEOMETRYCOLLECTION
        assert!(matches!(bbox_of(&v), Err(WkbBboxError::UnsupportedType(7))));
    }

    #[test]
    fn truncated_rejected() {
        let mut v = point_le(0.0, 0.0);
        v.truncate(v.len() - 4);
        assert!(matches!(bbox_of(&v), Err(WkbBboxError::Truncated)));
    }

    #[test]
    fn skips_zm_ordinates() {
        // 3D point: endian + (POINT | Z), x, y, z
        let mut v = vec![1u8];
        v.extend_from_slice(&(1u32 | EWKB_Z_FLAG).to_le_bytes());
        v.extend_from_slice(&1.0_f64.to_le_bytes());
        v.extend_from_slice(&2.0_f64.to_le_bytes());
        v.extend_from_slice(&9.0_f64.to_le_bytes()); // z, must be ignored
        let bb = bbox_of(&v).unwrap();
        assert_eq!((bb.min_x, bb.min_y, bb.max_x, bb.max_y), (1.0, 2.0, 1.0, 2.0));
    }
}

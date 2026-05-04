//! minimal WKB / EWKB decoder. supports the six geom types we materialise:
//! point, linestring, polygon, multipoint, multilinestring, multipolygon.
//! handles both little- and big-endian byte order, and EWKB SRID-bearing
//! variants (postgis emits these via ST_AsEWKB / default ST_AsBinary in
//! postgis 3+ when SRID is set). geometry collections and unknown types
//! are rejected.

use mars_artifact::{FeatureGeom, GeomKind};

#[derive(Debug, thiserror::Error)]
pub enum WkbError {
    #[error("truncated WKB")]
    Truncated,
    #[error("invalid byte-order flag: {0}")]
    BadEndian(u8),
    #[error("unsupported geometry type code: {0}")]
    UnsupportedType(u32),
    #[error("nested SRID-bearing geometry inside container")]
    NestedSrid,
    #[error("srid mismatch: expected {expected}, got {actual}")]
    SridMismatch { expected: u32, actual: u32 },
}

const WKB_POINT: u32 = 1;
const WKB_LINESTRING: u32 = 2;
const WKB_POLYGON: u32 = 3;
const WKB_MULTIPOINT: u32 = 4;
const WKB_MULTILINESTRING: u32 = 5;
const WKB_MULTIPOLYGON: u32 = 6;
const EWKB_SRID_FLAG: u32 = 0x2000_0000;
const EWKB_Z_FLAG: u32 = 0x8000_0000;
const EWKB_M_FLAG: u32 = 0x4000_0000;

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], WkbError> {
        if self.pos + n > self.buf.len() {
            return Err(WkbError::Truncated);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, WkbError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self, le: bool) -> Result<u32, WkbError> {
        let b = self.take(4)?;
        let arr: [u8; 4] = [b[0], b[1], b[2], b[3]];
        Ok(if le {
            u32::from_le_bytes(arr)
        } else {
            u32::from_be_bytes(arr)
        })
    }
    fn f64(&mut self, le: bool) -> Result<f64, WkbError> {
        let b = self.take(8)?;
        let arr: [u8; 8] = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
        Ok(if le {
            f64::from_le_bytes(arr)
        } else {
            f64::from_be_bytes(arr)
        })
    }
}

fn read_geom(c: &mut Cursor<'_>, allow_srid: bool, expected_srid: Option<u32>) -> Result<GeomKind, WkbError> {
    let endian = c.u8()?;
    let le = match endian {
        1 => true,
        0 => false,
        other => return Err(WkbError::BadEndian(other)),
    };
    let raw_type = c.u32(le)?;
    let has_z = raw_type & EWKB_Z_FLAG != 0;
    let has_m = raw_type & EWKB_M_FLAG != 0;
    let has_srid = raw_type & EWKB_SRID_FLAG != 0;
    if has_srid {
        if !allow_srid {
            return Err(WkbError::NestedSrid);
        }
        let srid = c.u32(le)?;
        if let Some(expected) = expected_srid
            && srid != expected
        {
            return Err(WkbError::SridMismatch { expected, actual: srid });
        }
    }
    let gtype = raw_type & 0x0000_00FF;
    match gtype {
        WKB_POINT => {
            let x = c.f64(le)?;
            let y = c.f64(le)?;
            skip_zm(c, le, has_z, has_m)?;
            Ok(GeomKind::Point((x, y)))
        }
        WKB_LINESTRING => Ok(GeomKind::LineString(read_points(c, le, has_z, has_m)?)),
        WKB_POLYGON => {
            let nrings = c.u32(le)? as usize;
            let mut rings = Vec::with_capacity(nrings);
            for _ in 0..nrings {
                rings.push(read_points(c, le, has_z, has_m)?);
            }
            Ok(GeomKind::Polygon(rings))
        }
        WKB_MULTIPOINT => {
            let n = c.u32(le)? as usize;
            let mut pts = Vec::with_capacity(n);
            for _ in 0..n {
                match read_geom(c, false, None)? {
                    GeomKind::Point(p) => pts.push(p),
                    _ => return Err(WkbError::UnsupportedType(WKB_MULTIPOINT)),
                }
            }
            Ok(GeomKind::MultiPoint(pts))
        }
        WKB_MULTILINESTRING => {
            let n = c.u32(le)? as usize;
            let mut parts = Vec::with_capacity(n);
            for _ in 0..n {
                match read_geom(c, false, None)? {
                    GeomKind::LineString(v) => parts.push(v),
                    _ => return Err(WkbError::UnsupportedType(WKB_MULTILINESTRING)),
                }
            }
            Ok(GeomKind::MultiLineString(parts))
        }
        WKB_MULTIPOLYGON => {
            let n = c.u32(le)? as usize;
            let mut polys = Vec::with_capacity(n);
            for _ in 0..n {
                match read_geom(c, false, None)? {
                    GeomKind::Polygon(rings) => polys.push(rings),
                    _ => return Err(WkbError::UnsupportedType(WKB_MULTIPOLYGON)),
                }
            }
            Ok(GeomKind::MultiPolygon(polys))
        }
        other => Err(WkbError::UnsupportedType(other)),
    }
}

fn read_points(c: &mut Cursor<'_>, le: bool, has_z: bool, has_m: bool) -> Result<Vec<(f64, f64)>, WkbError> {
    let n = c.u32(le)? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let x = c.f64(le)?;
        let y = c.f64(le)?;
        skip_zm(c, le, has_z, has_m)?;
        out.push((x, y));
    }
    Ok(out)
}

fn skip_zm(c: &mut Cursor<'_>, le: bool, z: bool, m: bool) -> Result<(), WkbError> {
    if z {
        let _ = c.f64(le)?;
    }
    if m {
        let _ = c.f64(le)?;
    }
    Ok(())
}

/// decode WKB / EWKB into a `FeatureGeom`. computes the bbox over the geometry
/// in canonical-CRS units and stores it as f32 (artifact format is f32 bbox).
/// `expected_srid` is validated when the WKB carries an SRID (EWKB).
pub fn decode_feature(id: u64, wkb: &[u8], expected_srid: Option<u32>) -> Result<FeatureGeom, WkbError> {
    let mut c = Cursor::new(wkb);
    let geom = read_geom(&mut c, true, expected_srid)?;
    let bbox = geom_bbox(&geom);
    Ok(FeatureGeom { id, bbox, geom })
}

fn geom_bbox(g: &GeomKind) -> [f32; 4] {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let mut acc = |x: f64, y: f64| {
        if x < min_x {
            min_x = x;
        }
        if y < min_y {
            min_y = y;
        }
        if x > max_x {
            max_x = x;
        }
        if y > max_y {
            max_y = y;
        }
    };
    match g {
        GeomKind::Point((x, y)) => acc(*x, *y),
        GeomKind::LineString(v) | GeomKind::MultiPoint(v) => {
            for &(x, y) in v {
                acc(x, y);
            }
        }
        GeomKind::Polygon(rings) | GeomKind::MultiLineString(rings) => {
            for r in rings {
                for &(x, y) in r {
                    acc(x, y);
                }
            }
        }
        GeomKind::MultiPolygon(polys) => {
            for poly in polys {
                for r in poly {
                    for &(x, y) in r {
                        acc(x, y);
                    }
                }
            }
        }
    }
    if !min_x.is_finite() {
        // empty geometry; collapse to zero bbox
        return [0.0, 0.0, 0.0, 0.0];
    }
    [min_x as f32, min_y as f32, max_x as f32, max_y as f32]
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn point_le() -> Vec<u8> {
        let mut v = vec![1u8];
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&1.5f64.to_le_bytes());
        v.extend_from_slice(&2.5f64.to_le_bytes());
        v
    }

    #[test]
    fn decodes_point() {
        let f = decode_feature(7, &point_le(), None).unwrap();
        assert_eq!(f.id, 7);
        assert!(matches!(f.geom, GeomKind::Point((1.5, 2.5))));
        assert_eq!(f.bbox, [1.5, 2.5, 1.5, 2.5]);
    }

    #[test]
    fn rejects_unknown_type() {
        let mut v = vec![1u8];
        v.extend_from_slice(&99u32.to_le_bytes());
        assert!(matches!(
            decode_feature(0, &v, None),
            Err(WkbError::UnsupportedType(99))
        ));
    }
}

//! WKB / EWKB decoder yielding [`Bbox`] or [`FeatureGeom`].
//!
//! Allocates only what the geometry actually needs; walks the byte stream,
//! supports the six geometry types we care about (point, linestring, polygon,
//! multipoint, multilinestring, multipolygon) and skips Z/M ordinates.
//! Geometry collections, curves, and TIN are rejected.
//!
//! Lives in `mars-artifact` (alongside the internal varint geometry codec) so
//! the compiler and any future ingestion adapter can derive bboxes and
//! `FeatureGeom`s from raw PostGIS payloads without duplicating the WKB
//! walker per crate.

use mars_types::Bbox;

use crate::geometry::{Coord, FeatureGeom, GeomKind};

/// Errors raised while parsing WKB / EWKB.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WkbError {
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
    /// A non-finite coordinate (NaN or infinity) appeared in the payload.
    #[error("non-finite coordinate")]
    NonFiniteCoord,
}

// well-formed WKB nests Multi -> {Point, LineString, Polygon}, depth <= 2.
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

/// Bbox of an arbitrary supported WKB / EWKB geometry. Returns `Empty` for
/// geometries that contain no coordinates (zero-ring polygons, empty multi-*
/// containers).
pub fn wkb_bbox(wkb: &[u8]) -> Result<Bbox, WkbError> {
    let mut acc = BboxAcc::new();
    let mut cur = Cursor::new(wkb);
    walk_bbox(&mut cur, &mut acc, 0)?;
    acc.finish()
}

/// Centroid of an arbitrary supported WKB / EWKB geometry. Defined as the
/// midpoint of the geometry's axis-aligned bbox. Cheap (single bbox walk)
/// and consistent with the Hilbert keying used by the compiler — the curve
/// is parameterised over the binding's combined bbox extent, so the
/// centroid only ever needs to be a stable point inside the geometry's
/// envelope, not an area-weighted true centroid. LAZARUS Phase C.
pub fn wkb_centroid(wkb: &[u8]) -> Result<[f64; 2], WkbError> {
    let bbox = wkb_bbox(wkb)?;
    Ok([(bbox.min_x + bbox.max_x) * 0.5, (bbox.min_y + bbox.max_y) * 0.5])
}

/// Decode a WKB / EWKB geometry into a [`FeatureGeom`] tagged with the
/// source-supplied `user_id`. Bbox is computed in the same pass. The slot
/// (per-page primary key) is assigned later by the writer; `user_id` is
/// non-key data and is allowed to repeat across features.
pub fn wkb_to_feature_geom(wkb: &[u8], user_id: u64) -> Result<FeatureGeom, WkbError> {
    let mut acc = BboxAcc::new();
    let mut cur = Cursor::new(wkb);
    let geom = walk_geom(&mut cur, &mut acc, 0)?;
    let bb = acc.finish()?;
    Ok(FeatureGeom {
        user_id,
        bbox: [bb.min_x as f32, bb.min_y as f32, bb.max_x as f32, bb.max_y as f32],
        geom,
    })
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
        // wkb may carry NaN / infinity in malformed inputs; skip them silently
        // for bbox accumulation but the geom decoder rejects them via finite().
        if x.is_finite() && y.is_finite() {
            self.min_x = self.min_x.min(x);
            self.min_y = self.min_y.min(y);
            self.max_x = self.max_x.max(x);
            self.max_y = self.max_y.max(y);
            self.seen = true;
        }
    }
    fn finish(self) -> Result<Bbox, WkbError> {
        if !self.seen {
            return Err(WkbError::Empty);
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

fn read_header(c: &mut Cursor<'_>) -> Result<(bool, u32, bool, bool), WkbError> {
    let endian = c.u8()?;
    let le = match endian {
        1 => true,
        0 => false,
        other => return Err(WkbError::BadEndian(other)),
    };
    let raw = c.u32(le)?;
    let has_z = raw & EWKB_Z_FLAG != 0;
    let has_m = raw & EWKB_M_FLAG != 0;
    let has_srid = raw & EWKB_SRID_FLAG != 0;
    if has_srid {
        let _ = c.u32(le)?;
    }
    let gtype = raw & 0x0000_00FF;
    Ok((le, gtype, has_z, has_m))
}

// ---- bbox-only walker -----------------------------------------------------

fn walk_bbox(c: &mut Cursor<'_>, acc: &mut BboxAcc, depth: u32) -> Result<(), WkbError> {
    if depth > MAX_WKB_DEPTH {
        return Err(WkbError::TooDeep);
    }
    let (le, gtype, has_z, has_m) = read_header(c)?;
    match gtype {
        WKB_POINT => {
            let x = c.f64(le)?;
            let y = c.f64(le)?;
            skip_zm(c, le, has_z, has_m)?;
            acc.add(x, y);
        }
        WKB_LINESTRING => walk_bbox_points(c, le, has_z, has_m, acc)?,
        WKB_POLYGON => {
            let nrings = c.u32(le)? as usize;
            for _ in 0..nrings {
                walk_bbox_points(c, le, has_z, has_m, acc)?;
            }
        }
        WKB_MULTIPOINT | WKB_MULTILINESTRING | WKB_MULTIPOLYGON => {
            let n = c.u32(le)? as usize;
            for _ in 0..n {
                walk_bbox(c, acc, depth + 1)?;
            }
        }
        other => return Err(WkbError::UnsupportedType(other)),
    }
    Ok(())
}

fn walk_bbox_points(c: &mut Cursor<'_>, le: bool, has_z: bool, has_m: bool, acc: &mut BboxAcc) -> Result<(), WkbError> {
    let n = c.u32(le)? as usize;
    for _ in 0..n {
        let x = c.f64(le)?;
        let y = c.f64(le)?;
        skip_zm(c, le, has_z, has_m)?;
        acc.add(x, y);
    }
    Ok(())
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

// ---- geom decoder ---------------------------------------------------------

fn read_coord(c: &mut Cursor<'_>, le: bool, has_z: bool, has_m: bool, acc: &mut BboxAcc) -> Result<Coord, WkbError> {
    let x = c.f64(le)?;
    let y = c.f64(le)?;
    skip_zm(c, le, has_z, has_m)?;
    if !x.is_finite() || !y.is_finite() {
        return Err(WkbError::NonFiniteCoord);
    }
    acc.add(x, y);
    Ok((x, y))
}

fn read_ring(
    c: &mut Cursor<'_>,
    le: bool,
    has_z: bool,
    has_m: bool,
    acc: &mut BboxAcc,
) -> Result<Vec<Coord>, WkbError> {
    let n = c.u32(le)? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(read_coord(c, le, has_z, has_m, acc)?);
    }
    Ok(out)
}

fn walk_geom(c: &mut Cursor<'_>, acc: &mut BboxAcc, depth: u32) -> Result<GeomKind, WkbError> {
    if depth > MAX_WKB_DEPTH {
        return Err(WkbError::TooDeep);
    }
    let (le, gtype, has_z, has_m) = read_header(c)?;
    match gtype {
        WKB_POINT => Ok(GeomKind::Point(read_coord(c, le, has_z, has_m, acc)?)),
        WKB_LINESTRING => Ok(GeomKind::LineString(read_ring(c, le, has_z, has_m, acc)?)),
        WKB_POLYGON => {
            let nrings = c.u32(le)? as usize;
            let mut rings = Vec::with_capacity(nrings);
            for _ in 0..nrings {
                rings.push(read_ring(c, le, has_z, has_m, acc)?);
            }
            Ok(GeomKind::Polygon(rings))
        }
        WKB_MULTIPOINT => {
            let n = c.u32(le)? as usize;
            let mut points = Vec::with_capacity(n);
            for _ in 0..n {
                match walk_geom(c, acc, depth + 1)? {
                    GeomKind::Point(p) => points.push(p),
                    _ => return Err(WkbError::UnsupportedType(WKB_MULTIPOINT)),
                }
            }
            Ok(GeomKind::MultiPoint(points))
        }
        WKB_MULTILINESTRING => {
            let n = c.u32(le)? as usize;
            let mut lines = Vec::with_capacity(n);
            for _ in 0..n {
                match walk_geom(c, acc, depth + 1)? {
                    GeomKind::LineString(l) => lines.push(l),
                    _ => return Err(WkbError::UnsupportedType(WKB_MULTILINESTRING)),
                }
            }
            Ok(GeomKind::MultiLineString(lines))
        }
        WKB_MULTIPOLYGON => {
            let n = c.u32(le)? as usize;
            let mut polys = Vec::with_capacity(n);
            for _ in 0..n {
                match walk_geom(c, acc, depth + 1)? {
                    GeomKind::Polygon(p) => polys.push(p),
                    _ => return Err(WkbError::UnsupportedType(WKB_MULTIPOLYGON)),
                }
            }
            Ok(GeomKind::MultiPolygon(polys))
        }
        other => Err(WkbError::UnsupportedType(other)),
    }
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

    #[test]
    fn point_decode() {
        let g = wkb_to_feature_geom(&point_le(1.5, 2.5), 7).unwrap();
        assert_eq!(g.user_id, 7);
        assert!(matches!(g.geom, GeomKind::Point((1.5, 2.5))));
    }

    #[test]
    fn linestring_decode() {
        let g = wkb_to_feature_geom(&linestring_le(&[(0.0, 0.0), (1.0, 2.0)]), 1).unwrap();
        match g.geom {
            GeomKind::LineString(coords) => assert_eq!(coords, vec![(0.0, 0.0), (1.0, 2.0)]),
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(g.bbox, [0.0_f32, 0.0, 1.0, 2.0]);
    }

    #[test]
    fn polygon_decode_multi_ring() {
        let p = polygon_le(&[
            &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)],
            &[(2.0, 2.0), (8.0, 2.0), (8.0, 8.0), (2.0, 8.0), (2.0, 2.0)],
        ]);
        let g = wkb_to_feature_geom(&p, 11).unwrap();
        match g.geom {
            GeomKind::Polygon(rings) => {
                assert_eq!(rings.len(), 2);
                assert_eq!(rings[0].len(), 5);
                assert_eq!(rings[1][0], (2.0, 2.0));
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(g.bbox[2], 10.0);
    }

    #[test]
    fn multipolygon_decode() {
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
        let g = wkb_to_feature_geom(&v, 1).unwrap();
        match g.geom {
            GeomKind::MultiPolygon(polys) => assert_eq!(polys.len(), 2),
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(g.bbox, [0.0_f32, 0.0, 12.0, 12.0]);
    }

    #[test]
    fn bbox_only_works_too() {
        let bb = wkb_bbox(&point_le(3.0, 4.0)).unwrap();
        assert_eq!((bb.min_x, bb.min_y), (3.0, 4.0));
    }

    #[test]
    fn empty_polygon_errors() {
        assert!(matches!(wkb_to_feature_geom(&polygon_le(&[]), 0), Err(WkbError::Empty)));
    }

    #[test]
    fn unsupported_type_rejected() {
        let mut v = vec![1u8];
        v.extend_from_slice(&7u32.to_le_bytes());
        assert!(matches!(wkb_to_feature_geom(&v, 0), Err(WkbError::UnsupportedType(7))));
    }

    #[test]
    fn truncated_rejected() {
        let mut v = point_le(0.0, 0.0);
        v.truncate(v.len() - 4);
        assert!(matches!(wkb_to_feature_geom(&v, 0), Err(WkbError::Truncated)));
    }

    #[test]
    fn wkb_centroid_returns_bbox_midpoint() {
        // point centroid is the point itself.
        assert_eq!(wkb_centroid(&point_le(7.0, 7.0)).unwrap(), [7.0, 7.0]);
        // linestring centroid is the midpoint of the bbox, not the path centroid.
        let centroid = wkb_centroid(&linestring_le(&[(0.0, 0.0), (10.0, 4.0)])).unwrap();
        assert_eq!(centroid, [5.0, 2.0]);
    }

    #[test]
    fn wkb_centroid_propagates_empty() {
        assert!(matches!(wkb_centroid(&polygon_le(&[])), Err(WkbError::Empty)));
    }

    #[test]
    fn ewkb_with_srid_decodes() {
        let mut v = vec![1u8];
        v.extend_from_slice(&(1u32 | EWKB_SRID_FLAG).to_le_bytes());
        v.extend_from_slice(&25832u32.to_le_bytes());
        v.extend_from_slice(&5.0_f64.to_le_bytes());
        v.extend_from_slice(&6.0_f64.to_le_bytes());
        let g = wkb_to_feature_geom(&v, 0).unwrap();
        assert!(matches!(g.geom, GeomKind::Point((5.0, 6.0))));
    }
}

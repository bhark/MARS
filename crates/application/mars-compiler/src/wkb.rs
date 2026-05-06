//! minimal WKB / EWKB decoder. supports the six geom types we materialise:
//! point, linestring, polygon, multipoint, multilinestring, multipolygon.
//! handles both little- and big-endian byte order, and EWKB SRID-bearing
//! variants (postgis emits these via ST_AsEWKB / default ST_AsBinary in
//! postgis 3+ when SRID is set). geometry collections and unknown types
//! are rejected.

use mars_artifact::{ArtifactError, FeatureGeom, GeomKind, GeomPayloadBuilder, GeomType};

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
    #[error("artifact codec rejected streamed geometry: {0}")]
    Artifact(#[from] ArtifactError),
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

/// stream a WKB blob directly into the geometry-payload builder. avoids the
/// `Vec<FeatureGeom>` + `Vec<Coord>`-per-ring intermediate that
/// [`decode_feature`] + [`mars_artifact::encode_geometry_payload`] allocate.
/// produces byte-identical output for the same logical feature.
pub fn write_into(
    builder: &mut GeomPayloadBuilder,
    id: u64,
    wkb: &[u8],
    expected_srid: Option<u32>,
) -> Result<(), WkbError> {
    let mut c = Cursor::new(wkb);
    let (le, has_z, has_m, gtype) = read_header(&mut c, true, expected_srid)?;
    match gtype {
        WKB_POINT => {
            let x = c.f64(le)?;
            let y = c.f64(le)?;
            skip_zm(&mut c, le, has_z, has_m)?;
            let mut fw = builder.begin(id, GeomType::Point)?;
            fw.coord_abs(x, y)?;
            fw.end()?;
        }
        WKB_LINESTRING => {
            let n = c.u32(le)? as usize;
            let mut fw = builder.begin(id, GeomType::LineString)?;
            fw.count(n)?;
            for _ in 0..n {
                let x = c.f64(le)?;
                let y = c.f64(le)?;
                skip_zm(&mut c, le, has_z, has_m)?;
                fw.coord_delta(x, y)?;
            }
            fw.end()?;
        }
        WKB_POLYGON => {
            let nrings = c.u32(le)? as usize;
            let mut fw = builder.begin(id, GeomType::Polygon)?;
            fw.count(nrings)?;
            for _ in 0..nrings {
                let n = c.u32(le)? as usize;
                fw.count(n)?;
                fw.reset_ring();
                for _ in 0..n {
                    let x = c.f64(le)?;
                    let y = c.f64(le)?;
                    skip_zm(&mut c, le, has_z, has_m)?;
                    fw.coord_delta(x, y)?;
                }
            }
            fw.end()?;
        }
        WKB_MULTIPOINT => {
            let n = c.u32(le)? as usize;
            let mut fw = builder.begin(id, GeomType::MultiPoint)?;
            fw.count(n)?;
            for _ in 0..n {
                // each sub-point carries its own header
                let (sub_le, sub_z, sub_m, sub_t) = read_header(&mut c, false, None)?;
                if sub_t != WKB_POINT {
                    return Err(WkbError::UnsupportedType(WKB_MULTIPOINT));
                }
                let x = c.f64(sub_le)?;
                let y = c.f64(sub_le)?;
                skip_zm(&mut c, sub_le, sub_z, sub_m)?;
                fw.coord_abs(x, y)?;
            }
            fw.end()?;
        }
        WKB_MULTILINESTRING => {
            let n = c.u32(le)? as usize;
            let mut fw = builder.begin(id, GeomType::MultiLineString)?;
            fw.count(n)?;
            for _ in 0..n {
                let (sub_le, sub_z, sub_m, sub_t) = read_header(&mut c, false, None)?;
                if sub_t != WKB_LINESTRING {
                    return Err(WkbError::UnsupportedType(WKB_MULTILINESTRING));
                }
                let m = c.u32(sub_le)? as usize;
                fw.count(m)?;
                fw.reset_ring();
                for _ in 0..m {
                    let x = c.f64(sub_le)?;
                    let y = c.f64(sub_le)?;
                    skip_zm(&mut c, sub_le, sub_z, sub_m)?;
                    fw.coord_delta(x, y)?;
                }
            }
            fw.end()?;
        }
        WKB_MULTIPOLYGON => {
            let n = c.u32(le)? as usize;
            let mut fw = builder.begin(id, GeomType::MultiPolygon)?;
            fw.count(n)?;
            for _ in 0..n {
                let (sub_le, sub_z, sub_m, sub_t) = read_header(&mut c, false, None)?;
                if sub_t != WKB_POLYGON {
                    return Err(WkbError::UnsupportedType(WKB_MULTIPOLYGON));
                }
                let nrings = c.u32(sub_le)? as usize;
                fw.count(nrings)?;
                for _ in 0..nrings {
                    let m = c.u32(sub_le)? as usize;
                    fw.count(m)?;
                    fw.reset_ring();
                    for _ in 0..m {
                        let x = c.f64(sub_le)?;
                        let y = c.f64(sub_le)?;
                        skip_zm(&mut c, sub_le, sub_z, sub_m)?;
                        fw.coord_delta(x, y)?;
                    }
                }
            }
            fw.end()?;
        }
        other => return Err(WkbError::UnsupportedType(other)),
    }
    Ok(())
}

/// shared header reader used by both [`decode_feature`] and [`write_into`].
/// returns `(little_endian, has_z, has_m, base_geom_type)`.
fn read_header(
    c: &mut Cursor<'_>,
    allow_srid: bool,
    expected_srid: Option<u32>,
) -> Result<(bool, bool, bool, u32), WkbError> {
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
    Ok((le, has_z, has_m, raw_type & 0x0000_00FF))
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

    fn point_be() -> Vec<u8> {
        let mut v = vec![0u8];
        v.extend_from_slice(&1u32.to_be_bytes());
        v.extend_from_slice(&1.5f64.to_be_bytes());
        v.extend_from_slice(&2.5f64.to_be_bytes());
        v
    }

    fn ewkb_point_le(srid: u32) -> Vec<u8> {
        let mut v = vec![1u8];
        v.extend_from_slice(&(1u32 | 0x2000_0000).to_le_bytes());
        v.extend_from_slice(&srid.to_le_bytes());
        v.extend_from_slice(&1.5f64.to_le_bytes());
        v.extend_from_slice(&2.5f64.to_le_bytes());
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

    fn multipolygon_le(polygons: &[&[&[(f64, f64)]]]) -> Vec<u8> {
        let mut v = vec![1u8];
        v.extend_from_slice(&6u32.to_le_bytes());
        v.extend_from_slice(&(polygons.len() as u32).to_le_bytes());
        for poly in polygons {
            v.extend_from_slice(&polygon_le(poly));
        }
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
    fn decodes_point_big_endian() {
        let f = decode_feature(0, &point_be(), None).unwrap();
        assert!(matches!(f.geom, GeomKind::Point((1.5, 2.5))));
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

    #[test]
    fn decodes_ewkb_with_matching_srid() {
        let f = decode_feature(0, &ewkb_point_le(25832), Some(25832)).unwrap();
        assert!(matches!(f.geom, GeomKind::Point((1.5, 2.5))));
    }

    #[test]
    fn rejects_ewkb_srid_mismatch() {
        let err = decode_feature(0, &ewkb_point_le(25832), Some(4326)).unwrap_err();
        assert!(matches!(
            err,
            WkbError::SridMismatch {
                expected: 4326,
                actual: 25832
            }
        ));
    }

    #[test]
    fn decodes_ewkb_without_expected_srid() {
        // no expected_srid means we accept any SRID
        let f = decode_feature(0, &ewkb_point_le(9999), None).unwrap();
        assert!(matches!(f.geom, GeomKind::Point((1.5, 2.5))));
    }

    #[test]
    fn skips_zm_flags() {
        // point with Z and M flags set
        let mut v = vec![1u8];
        v.extend_from_slice(&(1u32 | 0x8000_0000 | 0x4000_0000).to_le_bytes());
        v.extend_from_slice(&1.0f64.to_le_bytes());
        v.extend_from_slice(&2.0f64.to_le_bytes());
        v.extend_from_slice(&3.0f64.to_le_bytes()); // Z
        v.extend_from_slice(&4.0f64.to_le_bytes()); // M
        let f = decode_feature(0, &v, None).unwrap();
        assert!(matches!(f.geom, GeomKind::Point((1.0, 2.0))));
    }

    #[test]
    fn truncated_header() {
        let v = vec![1u8, 0x01, 0x00]; // too short for type
        assert!(matches!(decode_feature(0, &v, None), Err(WkbError::Truncated)));
    }

    #[test]
    fn truncated_point_coords() {
        let mut v = point_le();
        v.truncate(v.len() - 4); // cut inside second f64
        assert!(matches!(decode_feature(0, &v, None), Err(WkbError::Truncated)));
    }

    #[test]
    fn truncated_linestring_count() {
        let mut v = linestring_le(&[(0.0, 0.0), (1.0, 1.0)]);
        v.truncate(5); // after endian + type
        assert!(matches!(decode_feature(0, &v, None), Err(WkbError::Truncated)));
    }

    #[test]
    fn truncated_polygon_ring() {
        let mut v = polygon_le(&[&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]]);
        v.truncate(v.len() - 10); // inside last coordinate
        assert!(matches!(decode_feature(0, &v, None), Err(WkbError::Truncated)));
    }

    #[test]
    fn empty_geometry_bbox_is_zero() {
        let v = polygon_le(&[]); // polygon with 0 rings
        let f = decode_feature(0, &v, None).unwrap();
        assert_eq!(f.bbox, [0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn empty_multipolygon_bbox_is_zero() {
        let v = multipolygon_le(&[]);
        let f = decode_feature(0, &v, None).unwrap();
        assert_eq!(f.bbox, [0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn rejects_nested_srid_in_multipolygon() {
        // build a multipolygon containing a polygon with SRID flag
        let _inner = ewkb_point_le(25832); // reuse helper: this is actually a point, not polygon
        // build proper multipolygon with inner EWKB polygon
        let mut poly_with_srid = vec![1u8];
        poly_with_srid.extend_from_slice(&(3u32 | 0x2000_0000).to_le_bytes());
        poly_with_srid.extend_from_slice(&25832u32.to_le_bytes());
        poly_with_srid.extend_from_slice(&1u32.to_le_bytes()); // 1 ring
        poly_with_srid.extend_from_slice(&4u32.to_le_bytes()); // 4 points
        for (x, y) in [
            (0.0_f64, 0.0_f64),
            (1.0_f64, 0.0_f64),
            (1.0_f64, 1.0_f64),
            (0.0_f64, 0.0_f64),
        ] {
            poly_with_srid.extend_from_slice(&x.to_le_bytes());
            poly_with_srid.extend_from_slice(&y.to_le_bytes());
        }

        let mut v = vec![1u8];
        v.extend_from_slice(&6u32.to_le_bytes()); // multipolygon
        v.extend_from_slice(&1u32.to_le_bytes()); // 1 polygon
        v.extend_from_slice(&poly_with_srid);

        assert!(matches!(decode_feature(0, &v, None), Err(WkbError::NestedSrid)));
    }

    #[test]
    fn rejects_mismatched_type_in_multipolygon() {
        // multipolygon containing a linestring instead of polygon
        let mut v = vec![1u8];
        v.extend_from_slice(&6u32.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&linestring_le(&[(0.0, 0.0), (1.0, 1.0)]));
        assert!(matches!(decode_feature(0, &v, None), Err(WkbError::UnsupportedType(6))));
    }

    #[test]
    fn decodes_linestring_multilinestring_multipoint() {
        let ls = linestring_le(&[(0.0, 0.0), (1.0, 1.0), (2.0, 0.0)]);
        let f = decode_feature(0, &ls, None).unwrap();
        assert!(matches!(f.geom, GeomKind::LineString(ref pts) if pts.len() == 3));

        // multilinestring wrapping the linestring
        let mut mls = vec![1u8];
        mls.extend_from_slice(&5u32.to_le_bytes());
        mls.extend_from_slice(&1u32.to_le_bytes());
        mls.extend_from_slice(&ls);
        let f = decode_feature(0, &mls, None).unwrap();
        assert!(matches!(f.geom, GeomKind::MultiLineString(ref parts) if parts.len() == 1 && parts[0].len() == 3));

        // multipoint
        let mut mp = vec![1u8];
        mp.extend_from_slice(&4u32.to_le_bytes());
        mp.extend_from_slice(&2u32.to_le_bytes());
        for (x, y) in [(0.0_f64, 0.0_f64), (1.0_f64, 1.0_f64)] {
            mp.push(1u8); // point header
            mp.extend_from_slice(&1u32.to_le_bytes());
            mp.extend_from_slice(&x.to_le_bytes());
            mp.extend_from_slice(&y.to_le_bytes());
        }
        let f = decode_feature(0, &mp, None).unwrap();
        assert!(matches!(f.geom, GeomKind::MultiPoint(ref pts) if pts.len() == 2));
    }

    /// asserts the streaming `write_into` path produces a byte-identical
    /// geometry-payload section to the bulk `decode_feature + encode_geometry_payload`
    /// pipeline. covers each geom variant.
    #[test]
    fn streaming_matches_bulk_byte_for_byte() {
        use mars_artifact::encode_geometry_payload;

        // same fixture set, drives both pipelines.
        let multipoint = {
            let mut v = vec![1u8];
            v.extend_from_slice(&4u32.to_le_bytes());
            v.extend_from_slice(&3u32.to_le_bytes());
            for (x, y) in [(1.0_f64, 2.0_f64), (3.0, 4.0), (5.5, -6.5)] {
                v.push(1u8);
                v.extend_from_slice(&1u32.to_le_bytes());
                v.extend_from_slice(&x.to_le_bytes());
                v.extend_from_slice(&y.to_le_bytes());
            }
            v
        };
        let mls = {
            let inner = linestring_le(&[(0.0, 0.0), (10.0, 5.0), (20.0, -5.0)]);
            let mut v = vec![1u8];
            v.extend_from_slice(&5u32.to_le_bytes());
            v.extend_from_slice(&1u32.to_le_bytes());
            v.extend_from_slice(&inner);
            v
        };
        let cases: Vec<(u64, Vec<u8>)> = vec![
            (1, point_le()),
            (2, linestring_le(&[(0.0, 0.0), (1.0, 1.5), (2.5, 0.5)])),
            (
                3,
                polygon_le(&[
                    &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)],
                    &[(2.0, 2.0), (4.0, 2.0), (4.0, 4.0), (2.0, 4.0), (2.0, 2.0)],
                ]),
            ),
            (4, multipoint),
            (5, mls),
            (
                6,
                multipolygon_le(&[
                    &[&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]],
                    &[&[(5.0, 5.0), (6.0, 5.0), (6.0, 6.0), (5.0, 5.0)]],
                ]),
            ),
        ];

        // bulk pipeline: decode each WKB to FeatureGeom, then encode in one shot.
        let mut features = Vec::with_capacity(cases.len());
        for (id, wkb) in &cases {
            features.push(decode_feature(*id, wkb, None).unwrap());
        }
        let bulk = encode_geometry_payload(&features).unwrap();

        // streaming pipeline: write each WKB straight into the builder.
        let mut b = mars_artifact::GeomPayloadBuilder::new();
        for (id, wkb) in &cases {
            write_into(&mut b, *id, wkb, None).unwrap();
        }
        let streamed = b.finish().unwrap();

        assert_eq!(bulk.as_ref(), streamed.as_ref(), "streaming output diverged from bulk");
    }
}

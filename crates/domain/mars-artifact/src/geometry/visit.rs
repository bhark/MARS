use crate::ArtifactError;
use crate::varint::read_ivarint;

use super::{
    FeatureIndexEntry, GT_LINESTRING, GT_MULTILINESTRING, GT_MULTIPOINT, GT_MULTIPOLYGON, GT_POINT, GT_POLYGON,
    MAX_GEOM_COORDS, MAX_GEOM_PARTS,
    codec::{dequantize, read_uvarint_usize},
};

/// Streaming geometry visitor. Implementors receive coordinates one at a time
/// nested inside `begin_ring`/`end_ring` (closed polygon rings or open
/// linestring paths) and `begin_part`/`end_part` (one polygon, one linestring,
/// one point). Lets a renderer feed coords straight into a per-ring scratch
/// buffer without materialising the intermediate `GeomKind` tree.
///
/// Event shape per geometry type:
/// - `Point`: `begin_part`, `point`, `end_part`.
/// - `LineString`: `begin_part`, `begin_ring`, `point`*N, `end_ring`, `end_part`.
/// - `Polygon`: `begin_part`, then per ring `begin_ring`/`point`*N/`end_ring`, then `end_part`.
/// - `MultiPoint`: per point: `begin_part`, `point`, `end_part`.
/// - `MultiLineString`: per linestring: same as `LineString`.
/// - `MultiPolygon`: per polygon: same as `Polygon`.
pub trait GeomVisitor {
    fn point(&mut self, x: f64, y: f64);
    fn begin_ring(&mut self);
    fn end_ring(&mut self);
    fn begin_part(&mut self);
    fn end_part(&mut self);
}

/// Visitor counterpart of [`decode_one_geom`]. Decodes the same wire format
/// but emits visitor events instead of materialising a [`GeomKind`]. Generic
/// over the visitor so the inner varint loop monomorphises per impl — using
/// `&mut dyn GeomVisitor` here would erase the dispatch and undo the win.
pub fn visit_one_geom<V: GeomVisitor>(
    coord_area: &[u8],
    entry: &FeatureIndexEntry,
    v: &mut V,
) -> Result<(), ArtifactError> {
    let off = entry.coord_offset as usize;
    let end = off
        .checked_add(entry.coord_len as usize)
        .ok_or(ArtifactError::Truncated)?;
    if end > coord_area.len() {
        return Err(ArtifactError::Truncated);
    }
    let mut pos = off;
    visit_geom(entry.geom_type, coord_area, &mut pos, v)?;
    if pos != end {
        return Err(ArtifactError::Malformed("coord block length mismatch"));
    }
    Ok(())
}

fn visit_ring<V: GeomVisitor>(buf: &[u8], pos: &mut usize, v: &mut V) -> Result<(), ArtifactError> {
    let n = read_uvarint_usize(buf, pos)?;
    if n > MAX_GEOM_COORDS {
        return Err(ArtifactError::Malformed("ring coordinate count exceeds limit"));
    }
    if n == 0 {
        return Ok(());
    }
    let mut px = read_ivarint(buf, pos)?;
    let mut py = read_ivarint(buf, pos)?;
    v.point(dequantize(px), dequantize(py));
    for _ in 1..n {
        let dx = read_ivarint(buf, pos)?;
        let dy = read_ivarint(buf, pos)?;
        px = px
            .checked_add(dx)
            .ok_or(ArtifactError::Malformed("coord delta overflow"))?;
        py = py
            .checked_add(dy)
            .ok_or(ArtifactError::Malformed("coord delta overflow"))?;
        v.point(dequantize(px), dequantize(py));
    }
    Ok(())
}

fn visit_geom<V: GeomVisitor>(geom_type: u8, buf: &[u8], pos: &mut usize, v: &mut V) -> Result<(), ArtifactError> {
    match geom_type {
        GT_POINT => {
            let x = read_ivarint(buf, pos)?;
            let y = read_ivarint(buf, pos)?;
            v.begin_part();
            v.point(dequantize(x), dequantize(y));
            v.end_part();
        }
        GT_LINESTRING => {
            v.begin_part();
            v.begin_ring();
            visit_ring(buf, pos, v)?;
            v.end_ring();
            v.end_part();
        }
        GT_POLYGON => {
            let n = read_uvarint_usize(buf, pos)?;
            if n > MAX_GEOM_PARTS {
                return Err(ArtifactError::Malformed("polygon ring count exceeds limit"));
            }
            v.begin_part();
            for _ in 0..n {
                v.begin_ring();
                visit_ring(buf, pos, v)?;
                v.end_ring();
            }
            v.end_part();
        }
        GT_MULTIPOINT => {
            let n = read_uvarint_usize(buf, pos)?;
            if n > MAX_GEOM_COORDS {
                return Err(ArtifactError::Malformed("multipoint count exceeds limit"));
            }
            for _ in 0..n {
                let x = read_ivarint(buf, pos)?;
                let y = read_ivarint(buf, pos)?;
                v.begin_part();
                v.point(dequantize(x), dequantize(y));
                v.end_part();
            }
        }
        GT_MULTILINESTRING => {
            let n = read_uvarint_usize(buf, pos)?;
            if n > MAX_GEOM_PARTS {
                return Err(ArtifactError::Malformed("multilinestring part count exceeds limit"));
            }
            for _ in 0..n {
                v.begin_part();
                v.begin_ring();
                visit_ring(buf, pos, v)?;
                v.end_ring();
                v.end_part();
            }
        }
        GT_MULTIPOLYGON => {
            let n = read_uvarint_usize(buf, pos)?;
            if n > MAX_GEOM_PARTS {
                return Err(ArtifactError::Malformed("multipolygon count exceeds limit"));
            }
            for _ in 0..n {
                let m = read_uvarint_usize(buf, pos)?;
                if m > MAX_GEOM_PARTS {
                    return Err(ArtifactError::Malformed("multipolygon ring count exceeds limit"));
                }
                v.begin_part();
                for _ in 0..m {
                    v.begin_ring();
                    visit_ring(buf, pos, v)?;
                    v.end_ring();
                }
                v.end_part();
            }
        }
        _ => return Err(ArtifactError::Malformed("bad geom_type")),
    }
    Ok(())
}

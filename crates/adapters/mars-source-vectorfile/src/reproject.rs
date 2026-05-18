//! WKB reprojection: walk a well-known-binary geometry, transforming every
//! coordinate pair from `source_crs` to `target_crs` in place.
//!
//! Implements the OGC simple-features WKB types (Point, LineString,
//! Polygon, MultiPoint, MultiLineString, MultiPolygon, GeometryCollection)
//! for both XDR (big-endian) and NDR (little-endian) byte orders.
//!
//! Out of scope: SRID-prefixed EWKB, 3D/4D coordinates (Z/M). Vector-file
//! formats this adapter consumes (FGB, GeoJSON) emit 2D OGC WKB through
//! geozero, so the supported surface matches what we'll actually see.

use std::rc::Rc;

use bytes::Bytes;
use mars_proj::Transformer;
use mars_types::CrsCode;

use crate::error::ReprojectError;

const WKB_POINT: u32 = 1;
const WKB_LINESTRING: u32 = 2;
const WKB_POLYGON: u32 = 3;
const WKB_MULTIPOINT: u32 = 4;
const WKB_MULTILINESTRING: u32 = 5;
const WKB_MULTIPOLYGON: u32 = 6;
const WKB_GEOMETRYCOLLECTION: u32 = 7;

/// Reproject a WKB geometry. Returns a fresh `Bytes` with all coordinate
/// pairs transformed; structure (rings, parts) is preserved verbatim.
pub fn reproject_wkb(wkb: &[u8], xform: &Transformer) -> Result<Bytes, ReprojectError> {
    // collect all (x,y) into one flat buffer, transform in one ffi call,
    // then write a fresh wkb with the new coords stitched back in. avoids
    // per-point ffi hops on large rings.
    let mut coords: Vec<[f64; 2]> = Vec::new();
    let layout = parse_layout(wkb, &mut coords)?;
    xform.transform_points(&mut coords).map_err(ReprojectError::Proj)?;
    let mut out = Vec::with_capacity(wkb.len());
    write_with_coords(&layout, &coords, &mut out);
    Ok(Bytes::from(out))
}

/// Resolve a transformer for `from -> to`. Wraps `mars_proj::cached_transformer`.
pub fn transformer(from: &CrsCode, to: &CrsCode) -> Result<Rc<Transformer>, ReprojectError> {
    mars_proj::cached_transformer(from, to).map_err(ReprojectError::Proj)
}

#[derive(Debug)]
struct Layout {
    nodes: Vec<Node>,
}

#[derive(Debug)]
enum Node {
    Point {
        little_endian: bool,
    },
    LineString {
        little_endian: bool,
        n: u32,
    },
    Polygon {
        little_endian: bool,
        ring_sizes: Vec<u32>,
    },
    MultiPoint {
        little_endian: bool,
        headers: Vec<bool>,
    },
    MultiLineString {
        little_endian: bool,
        parts: Vec<(bool, u32)>,
    },
    MultiPolygon {
        little_endian: bool,
        parts: Vec<(bool, Vec<u32>)>,
    },
}

fn parse_layout(wkb: &[u8], coords: &mut Vec<[f64; 2]>) -> Result<Layout, ReprojectError> {
    let mut cur = Cursor::new(wkb);
    let mut nodes = Vec::new();
    parse_geometry(&mut cur, coords, &mut nodes)?;
    Ok(Layout { nodes })
}

fn parse_geometry(
    cur: &mut Cursor<'_>,
    coords: &mut Vec<[f64; 2]>,
    nodes: &mut Vec<Node>,
) -> Result<(), ReprojectError> {
    let le = cur.read_u8()? != 0;
    let ty = cur.read_u32(le)?;
    // strip optional 3d/4d flags. we don't support emitting Z/M; flag them.
    let base = ty & 0x000F_FFFF;
    if (ty & 0xFFF0_0000) != 0 {
        return Err(ReprojectError::Wkb(format!(
            "wkb modifiers (Z/M/SRID) not supported: type=0x{ty:x}"
        )));
    }
    match base {
        WKB_POINT => {
            let x = cur.read_f64(le)?;
            let y = cur.read_f64(le)?;
            coords.push([x, y]);
            nodes.push(Node::Point { little_endian: le });
        }
        WKB_LINESTRING => {
            let n = cur.read_u32(le)?;
            for _ in 0..n {
                let x = cur.read_f64(le)?;
                let y = cur.read_f64(le)?;
                coords.push([x, y]);
            }
            nodes.push(Node::LineString { little_endian: le, n });
        }
        WKB_POLYGON => {
            let rings = cur.read_u32(le)?;
            let mut ring_sizes = Vec::with_capacity(rings as usize);
            for _ in 0..rings {
                let n = cur.read_u32(le)?;
                ring_sizes.push(n);
                for _ in 0..n {
                    let x = cur.read_f64(le)?;
                    let y = cur.read_f64(le)?;
                    coords.push([x, y]);
                }
            }
            nodes.push(Node::Polygon {
                little_endian: le,
                ring_sizes,
            });
        }
        WKB_MULTIPOINT => {
            let n = cur.read_u32(le)?;
            let mut headers = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let sub_le = cur.read_u8()? != 0;
                let sub_ty = cur.read_u32(sub_le)?;
                if (sub_ty & 0x000F_FFFF) != WKB_POINT {
                    return Err(ReprojectError::Wkb(format!(
                        "multipoint part has non-point type: 0x{sub_ty:x}"
                    )));
                }
                let x = cur.read_f64(sub_le)?;
                let y = cur.read_f64(sub_le)?;
                coords.push([x, y]);
                headers.push(sub_le);
            }
            nodes.push(Node::MultiPoint {
                little_endian: le,
                headers,
            });
        }
        WKB_MULTILINESTRING => {
            let n = cur.read_u32(le)?;
            let mut parts = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let sub_le = cur.read_u8()? != 0;
                let sub_ty = cur.read_u32(sub_le)?;
                if (sub_ty & 0x000F_FFFF) != WKB_LINESTRING {
                    return Err(ReprojectError::Wkb(format!("multilinestring part type 0x{sub_ty:x}")));
                }
                let pn = cur.read_u32(sub_le)?;
                for _ in 0..pn {
                    let x = cur.read_f64(sub_le)?;
                    let y = cur.read_f64(sub_le)?;
                    coords.push([x, y]);
                }
                parts.push((sub_le, pn));
            }
            nodes.push(Node::MultiLineString {
                little_endian: le,
                parts,
            });
        }
        WKB_MULTIPOLYGON => {
            let n = cur.read_u32(le)?;
            let mut parts = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let sub_le = cur.read_u8()? != 0;
                let sub_ty = cur.read_u32(sub_le)?;
                if (sub_ty & 0x000F_FFFF) != WKB_POLYGON {
                    return Err(ReprojectError::Wkb(format!("multipolygon part type 0x{sub_ty:x}")));
                }
                let rings = cur.read_u32(sub_le)?;
                let mut ring_sizes = Vec::with_capacity(rings as usize);
                for _ in 0..rings {
                    let m = cur.read_u32(sub_le)?;
                    ring_sizes.push(m);
                    for _ in 0..m {
                        let x = cur.read_f64(sub_le)?;
                        let y = cur.read_f64(sub_le)?;
                        coords.push([x, y]);
                    }
                }
                parts.push((sub_le, ring_sizes));
            }
            nodes.push(Node::MultiPolygon {
                little_endian: le,
                parts,
            });
        }
        WKB_GEOMETRYCOLLECTION => {
            return Err(ReprojectError::Wkb("GeometryCollection not supported".into()));
        }
        other => {
            return Err(ReprojectError::Wkb(format!("unknown wkb type: {other}")));
        }
    }
    Ok(())
}

fn write_with_coords(layout: &Layout, coords: &[[f64; 2]], out: &mut Vec<u8>) {
    let mut idx = 0usize;
    for node in &layout.nodes {
        match node {
            Node::Point { little_endian } => {
                push_header(out, *little_endian, WKB_POINT);
                push_point(out, *little_endian, coords[idx]);
                idx += 1;
            }
            Node::LineString { little_endian, n } => {
                push_header(out, *little_endian, WKB_LINESTRING);
                push_u32(out, *little_endian, *n);
                for _ in 0..*n {
                    push_point(out, *little_endian, coords[idx]);
                    idx += 1;
                }
            }
            Node::Polygon {
                little_endian,
                ring_sizes,
            } => {
                push_header(out, *little_endian, WKB_POLYGON);
                push_u32(out, *little_endian, ring_sizes.len() as u32);
                for &m in ring_sizes {
                    push_u32(out, *little_endian, m);
                    for _ in 0..m {
                        push_point(out, *little_endian, coords[idx]);
                        idx += 1;
                    }
                }
            }
            Node::MultiPoint { little_endian, headers } => {
                push_header(out, *little_endian, WKB_MULTIPOINT);
                push_u32(out, *little_endian, headers.len() as u32);
                for &sub_le in headers {
                    push_header(out, sub_le, WKB_POINT);
                    push_point(out, sub_le, coords[idx]);
                    idx += 1;
                }
            }
            Node::MultiLineString { little_endian, parts } => {
                push_header(out, *little_endian, WKB_MULTILINESTRING);
                push_u32(out, *little_endian, parts.len() as u32);
                for (sub_le, pn) in parts {
                    push_header(out, *sub_le, WKB_LINESTRING);
                    push_u32(out, *sub_le, *pn);
                    for _ in 0..*pn {
                        push_point(out, *sub_le, coords[idx]);
                        idx += 1;
                    }
                }
            }
            Node::MultiPolygon { little_endian, parts } => {
                push_header(out, *little_endian, WKB_MULTIPOLYGON);
                push_u32(out, *little_endian, parts.len() as u32);
                for (sub_le, ring_sizes) in parts {
                    push_header(out, *sub_le, WKB_POLYGON);
                    push_u32(out, *sub_le, ring_sizes.len() as u32);
                    for &m in ring_sizes {
                        push_u32(out, *sub_le, m);
                        for _ in 0..m {
                            push_point(out, *sub_le, coords[idx]);
                            idx += 1;
                        }
                    }
                }
            }
        }
    }
}

fn push_header(out: &mut Vec<u8>, little_endian: bool, ty: u32) {
    out.push(u8::from(little_endian));
    push_u32(out, little_endian, ty);
}

fn push_u32(out: &mut Vec<u8>, little_endian: bool, v: u32) {
    if little_endian {
        out.extend_from_slice(&v.to_le_bytes());
    } else {
        out.extend_from_slice(&v.to_be_bytes());
    }
}

fn push_point(out: &mut Vec<u8>, little_endian: bool, c: [f64; 2]) {
    if little_endian {
        out.extend_from_slice(&c[0].to_le_bytes());
        out.extend_from_slice(&c[1].to_le_bytes());
    } else {
        out.extend_from_slice(&c[0].to_be_bytes());
        out.extend_from_slice(&c[1].to_be_bytes());
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
    fn read_u8(&mut self) -> Result<u8, ReprojectError> {
        let b = *self
            .buf
            .get(self.pos)
            .ok_or_else(|| ReprojectError::Wkb("unexpected eof".into()))?;
        self.pos += 1;
        Ok(b)
    }
    fn read_u32(&mut self, le: bool) -> Result<u32, ReprojectError> {
        let slice = self
            .buf
            .get(self.pos..self.pos + 4)
            .ok_or_else(|| ReprojectError::Wkb("unexpected eof in u32".into()))?;
        self.pos += 4;
        let mut a = [0u8; 4];
        a.copy_from_slice(slice);
        Ok(if le {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        })
    }
    fn read_f64(&mut self, le: bool) -> Result<f64, ReprojectError> {
        let slice = self
            .buf
            .get(self.pos..self.pos + 8)
            .ok_or_else(|| ReprojectError::Wkb("unexpected eof in f64".into()))?;
        self.pos += 8;
        let mut a = [0u8; 8];
        a.copy_from_slice(slice);
        Ok(if le {
            f64::from_le_bytes(a)
        } else {
            f64::from_be_bytes(a)
        })
    }
}

#[cfg(test)]
mod tests;

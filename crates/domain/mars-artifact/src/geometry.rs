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

pub(crate) const GT_POINT: u8 = 1;
pub(crate) const GT_LINESTRING: u8 = 2;
pub(crate) const GT_POLYGON: u8 = 3;
pub(crate) const GT_MULTIPOINT: u8 = 4;
pub(crate) const GT_MULTILINESTRING: u8 = 5;
pub(crate) const GT_MULTIPOLYGON: u8 = 6;

/// hard limit on coordinates per ring or points per multipoint.
pub(crate) const MAX_GEOM_COORDS: usize = 1_000_000;
/// hard limit on rings / parts / polygons per geometry.
pub(crate) const MAX_GEOM_PARTS: usize = 100_000;

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
pub(crate) const FEATURE_INDEX_ENTRY_LEN: usize = 8 + 4 * 4 + 1 + 4 + 4;

/// encode features into the geometry-payload section bytes.
///
/// Features must be sorted by `id` ascending (required for determinism and
/// for the index to support binary search). Violation returns
/// `ArtifactError::UnsortedFeatures` in release; debug builds also assert.
pub fn encode_geometry_payload(features: &[FeatureGeom]) -> Result<Bytes, ArtifactError> {
    if !features.windows(2).all(|w| w[0].id < w[1].id) {
        return Err(ArtifactError::UnsortedFeatures);
    }
    let mut b = GeomPayloadBuilder::new();
    for f in features {
        b.push_feature(f)?;
    }
    b.finish()
}

/// Streaming geometry-payload builder. Lets producers (e.g. a WKB decoder)
/// emit features one coord at a time, avoiding the `Vec<FeatureGeom>` +
/// `Vec<Coord>`-per-ring intermediate that [`encode_geometry_payload`]
/// requires. The on-wire bytes are byte-identical to the bulk encoder for
/// the same logical feature stream.
pub struct GeomPayloadBuilder {
    body: Vec<u8>,
    spans: Vec<(u32, u32)>,
    index: Vec<(u64, [f32; 4], u8)>,
    last_id: Option<u64>,
}

/// Geometry-type tag handed to [`GeomPayloadBuilder::begin`]. Kept separate
/// from the on-wire `u8` so callers can't pass an arbitrary byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeomType {
    Point,
    LineString,
    Polygon,
    MultiPoint,
    MultiLineString,
    MultiPolygon,
}

impl GeomType {
    #[inline]
    fn byte(self) -> u8 {
        match self {
            Self::Point => GT_POINT,
            Self::LineString => GT_LINESTRING,
            Self::Polygon => GT_POLYGON,
            Self::MultiPoint => GT_MULTIPOINT,
            Self::MultiLineString => GT_MULTILINESTRING,
            Self::MultiPolygon => GT_MULTIPOLYGON,
        }
    }
}

/// In-progress feature handed back from [`GeomPayloadBuilder::begin`].
/// Caller drives the format's structure: `count` writes a uvarint sub-count
/// (rings, parts, multi-counts), `reset_ring` restarts delta state at a new
/// ring boundary, `coord_delta` and `coord_abs` push coordinates. `end`
/// commits the feature; dropping without `end` rolls it back.
pub struct FeatureWriter<'a> {
    builder: &'a mut GeomPayloadBuilder,
    geom_type: u8,
    body_start: u32,
    px: i64,
    py: i64,
    have_anchor: bool,
    bbox: BboxAcc,
    id: u64,
    ended: bool,
}

#[derive(Default)]
struct BboxAcc {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    seen: bool,
}

impl BboxAcc {
    fn fold(&mut self, x: f64, y: f64) {
        if !self.seen {
            self.min_x = x;
            self.min_y = y;
            self.max_x = x;
            self.max_y = y;
            self.seen = true;
            return;
        }
        if x < self.min_x {
            self.min_x = x;
        }
        if y < self.min_y {
            self.min_y = y;
        }
        if x > self.max_x {
            self.max_x = x;
        }
        if y > self.max_y {
            self.max_y = y;
        }
    }
    fn snapshot(&self) -> [f32; 4] {
        if !self.seen {
            return [0.0, 0.0, 0.0, 0.0];
        }
        [
            self.min_x as f32,
            self.min_y as f32,
            self.max_x as f32,
            self.max_y as f32,
        ]
    }
}

impl Default for GeomPayloadBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl GeomPayloadBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            body: Vec::new(),
            spans: Vec::new(),
            index: Vec::new(),
            last_id: None,
        }
    }

    /// Start writing a feature with the given id and geom type. Returns a
    /// stateful writer the caller drives. `id` must be strictly greater than
    /// every previously written id.
    pub fn begin(&mut self, id: u64, geom_type: GeomType) -> Result<FeatureWriter<'_>, ArtifactError> {
        if let Some(prev) = self.last_id
            && id <= prev
        {
            return Err(ArtifactError::UnsortedFeatures);
        }
        let body_start =
            u32::try_from(self.body.len()).map_err(|_| ArtifactError::Malformed("geometry payload too large"))?;
        Ok(FeatureWriter {
            builder: self,
            geom_type: geom_type.byte(),
            body_start,
            px: 0,
            py: 0,
            have_anchor: false,
            bbox: BboxAcc::default(),
            id,
            ended: false,
        })
    }

    /// Convenience: append a fully-formed [`FeatureGeom`]. Used by the bulk
    /// encoder; producers that already have rows materialised may call this
    /// directly.
    pub fn push_feature(&mut self, f: &FeatureGeom) -> Result<(), ArtifactError> {
        if let Some(prev) = self.last_id
            && f.id <= prev
        {
            return Err(ArtifactError::UnsortedFeatures);
        }
        let start =
            u32::try_from(self.body.len()).map_err(|_| ArtifactError::Malformed("geometry payload too large"))?;
        write_geom(&mut self.body, &f.geom)?;
        let len = u32::try_from(self.body.len() - start as usize)
            .map_err(|_| ArtifactError::Malformed("geometry section too large"))?;
        self.spans.push((start, len));
        self.index.push((f.id, f.bbox, geom_type_byte(&f.geom)));
        self.last_id = Some(f.id);
        Ok(())
    }

    /// Finalise: emit the count + index header followed by the body bytes.
    pub fn finish(self) -> Result<Bytes, ArtifactError> {
        let count = u32::try_from(self.index.len()).map_err(|_| ArtifactError::Malformed("too many features"))?;
        let header_len = 4usize
            .checked_add(
                self.index
                    .len()
                    .checked_mul(FEATURE_INDEX_ENTRY_LEN)
                    .ok_or(ArtifactError::Malformed("geometry payload too large"))?,
            )
            .ok_or(ArtifactError::Malformed("geometry payload too large"))?;
        let total_len = header_len
            .checked_add(self.body.len())
            .ok_or(ArtifactError::Malformed("geometry payload too large"))?;
        let mut out = Vec::with_capacity(total_len);
        out.extend_from_slice(&count.to_le_bytes());
        for ((id, bbox, geom_type), (off, len)) in self.index.iter().zip(&self.spans) {
            out.extend_from_slice(&id.to_le_bytes());
            for v in bbox {
                out.extend_from_slice(&v.to_le_bytes());
            }
            out.push(*geom_type);
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
        }
        out.extend_from_slice(&self.body);
        Ok(Bytes::from(out))
    }
}

impl<'a> FeatureWriter<'a> {
    /// Push a uvarint sub-count (ring length, ring count, multi-count).
    pub fn count(&mut self, n: usize) -> Result<(), ArtifactError> {
        let v = u64::try_from(n).map_err(|_| ArtifactError::Malformed("count exceeds u64"))?;
        crate::varint::write_uvarint(&mut self.builder.body, v);
        Ok(())
    }

    /// Reset delta state at a new ring boundary. The next `coord_delta` will
    /// be written as an absolute (zigzag-encoded) ivarint.
    pub fn reset_ring(&mut self) {
        self.have_anchor = false;
        self.px = 0;
        self.py = 0;
    }

    /// Push a coordinate, delta-encoded against the previous coord in the
    /// current ring. The first coord in a ring is written absolute.
    pub fn coord_delta(&mut self, x: f64, y: f64) -> Result<(), ArtifactError> {
        let qx = quantize(x)?;
        let qy = quantize(y)?;
        if !self.have_anchor {
            crate::varint::write_ivarint(&mut self.builder.body, qx);
            crate::varint::write_ivarint(&mut self.builder.body, qy);
            self.px = qx;
            self.py = qy;
            self.have_anchor = true;
        } else {
            let dx = qx.checked_sub(self.px).ok_or(ArtifactError::CoordOutOfRange(x))?;
            let dy = qy.checked_sub(self.py).ok_or(ArtifactError::CoordOutOfRange(y))?;
            crate::varint::write_ivarint(&mut self.builder.body, dx);
            crate::varint::write_ivarint(&mut self.builder.body, dy);
            self.px = qx;
            self.py = qy;
        }
        self.bbox.fold(x, y);
        Ok(())
    }

    /// Push an absolute (non-delta) coordinate. Used by Point and MultiPoint
    /// per the on-wire layout.
    pub fn coord_abs(&mut self, x: f64, y: f64) -> Result<(), ArtifactError> {
        let qx = quantize(x)?;
        let qy = quantize(y)?;
        crate::varint::write_ivarint(&mut self.builder.body, qx);
        crate::varint::write_ivarint(&mut self.builder.body, qy);
        self.bbox.fold(x, y);
        Ok(())
    }

    /// Commit this feature to the index.
    pub fn end(mut self) -> Result<(), ArtifactError> {
        let body_end = u32::try_from(self.builder.body.len())
            .map_err(|_| ArtifactError::Malformed("geometry payload too large"))?;
        let len = body_end
            .checked_sub(self.body_start)
            .ok_or(ArtifactError::Malformed("geometry section too large"))?;
        self.builder.spans.push((self.body_start, len));
        self.builder.index.push((self.id, self.bbox.snapshot(), self.geom_type));
        self.builder.last_id = Some(self.id);
        self.ended = true;
        Ok(())
    }
}

impl Drop for FeatureWriter<'_> {
    fn drop(&mut self) {
        if !self.ended {
            // roll back any body bytes the caller wrote; the index entry was
            // never appended, so dropping mid-feature leaves the builder
            // consistent for retry.
            self.builder.body.truncate(self.body_start as usize);
        }
    }
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

/// One feature's index entry: id, approximate bbox, and pointer into the
/// coord area. Decoded lazily by [`FeatureIndexIter`]; coordinates stay
/// untouched until [`decode_one_geom`] is called for a chosen entry.
#[derive(Debug, Clone, Copy)]
pub struct FeatureIndexEntry {
    pub id: u64,
    pub bbox: [f32; 4],
    pub geom_type: u8,
    /// offset into the coord area (the bytes after the index header).
    pub coord_offset: u32,
    pub coord_len: u32,
}

impl FeatureIndexEntry {
    /// Decode the geometry kind from the on-wire `geom_type` byte. Returns a
    /// typed [`GeomType`] so callers can branch without re-knowing the byte
    /// constants.
    pub fn geom_kind(&self) -> Result<GeomType, ArtifactError> {
        match self.geom_type {
            GT_POINT => Ok(GeomType::Point),
            GT_LINESTRING => Ok(GeomType::LineString),
            GT_POLYGON => Ok(GeomType::Polygon),
            GT_MULTIPOINT => Ok(GeomType::MultiPoint),
            GT_MULTILINESTRING => Ok(GeomType::MultiLineString),
            GT_MULTIPOLYGON => Ok(GeomType::MultiPolygon),
            _ => Err(ArtifactError::Malformed("bad geom_type")),
        }
    }
}

/// Lazy iterator over the geometry-payload index. Cheap: each step copies
/// one 33-byte index entry. Coordinates are not touched.
pub struct FeatureIndexIter<'a> {
    bytes: &'a [u8],
    coord_area_len: usize,
    count: usize,
    pos: usize,
}

impl<'a> FeatureIndexIter<'a> {
    /// Bytes of the coord area (the region after the fixed-stride index
    /// header). Useful for callers that decode geometries on the fly.
    #[must_use]
    pub fn coord_area(&self) -> &'a [u8] {
        &self.bytes[self.bytes.len() - self.coord_area_len..]
    }

    /// Number of index entries in the payload.
    #[must_use]
    pub fn len(&self) -> usize {
        self.count
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

impl Iterator for FeatureIndexIter<'_> {
    type Item = Result<FeatureIndexEntry, ArtifactError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.count {
            return None;
        }
        let off = 4 + self.pos * FEATURE_INDEX_ENTRY_LEN;
        self.pos += 1;
        let entry = (|| -> Result<FeatureIndexEntry, ArtifactError> {
            let id = u64::from_le_bytes(read_array::<8>(self.bytes, off)?);
            let bbox = [
                f32::from_le_bytes(read_array::<4>(self.bytes, off + 8)?),
                f32::from_le_bytes(read_array::<4>(self.bytes, off + 12)?),
                f32::from_le_bytes(read_array::<4>(self.bytes, off + 16)?),
                f32::from_le_bytes(read_array::<4>(self.bytes, off + 20)?),
            ];
            let geom_type = self.bytes[off + 24];
            let coord_offset = u32::from_le_bytes(read_array::<4>(self.bytes, off + 25)?);
            let coord_len = u32::from_le_bytes(read_array::<4>(self.bytes, off + 29)?);
            // bound-check the entry up front so callers can decode by entry
            // without re-validating each time.
            let end = (coord_offset as usize)
                .checked_add(coord_len as usize)
                .ok_or(ArtifactError::Truncated)?;
            if end > self.coord_area_len {
                return Err(ArtifactError::Truncated);
            }
            Ok(FeatureIndexEntry {
                id,
                bbox,
                geom_type,
                coord_offset,
                coord_len,
            })
        })();
        Some(entry)
    }
}

/// Build a [`FeatureIndexIter`] over a geometry-payload section.
pub fn iter_feature_index(bytes: &[u8]) -> Result<FeatureIndexIter<'_>, ArtifactError> {
    let (count, header_len) = parse_payload_header(bytes)?;
    let coord_area_len = bytes.len() - header_len;
    Ok(FeatureIndexIter {
        bytes,
        coord_area_len,
        count,
        pos: 0,
    })
}

/// Decode the coordinates for a single index entry. The `coord_area` slice
/// must be the one returned by `FeatureIndexIter::coord_area`.
pub fn decode_one_geom(coord_area: &[u8], entry: &FeatureIndexEntry) -> Result<GeomKind, ArtifactError> {
    let off = entry.coord_offset as usize;
    let end = off
        .checked_add(entry.coord_len as usize)
        .ok_or(ArtifactError::Truncated)?;
    if end > coord_area.len() {
        return Err(ArtifactError::Truncated);
    }
    let mut pos = off;
    let geom = read_geom(entry.geom_type, coord_area, &mut pos)?;
    if pos != end {
        return Err(ArtifactError::Malformed("coord block length mismatch"));
    }
    Ok(geom)
}

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

/// Decode only features whose `(id, bbox)` predicate returns `true`.
/// Walks the cheap index, applies `pred`, and decodes coordinates only for
/// survivors. Equivalent in result to filtering the output of
/// [`decode_geometry_payload`], but avoids decoding skipped features.
pub fn decode_geometry_payload_filtered<F>(bytes: &[u8], mut pred: F) -> Result<Vec<FeatureGeom>, ArtifactError>
where
    F: FnMut(u64, [f32; 4]) -> bool,
{
    let iter = iter_feature_index(bytes)?;
    let coord_area = iter.coord_area();
    let mut out = Vec::new();
    for entry in iter {
        let entry = entry?;
        if !pred(entry.id, entry.bbox) {
            continue;
        }
        let geom = decode_one_geom(coord_area, &entry)?;
        out.push(FeatureGeom {
            id: entry.id,
            bbox: entry.bbox,
            geom,
        });
    }
    Ok(out)
}

fn parse_payload_header(bytes: &[u8]) -> Result<(usize, usize), ArtifactError> {
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
    Ok((count, header_len))
}

pub fn decode_geometry_payload(bytes: &[u8]) -> Result<Vec<FeatureGeom>, ArtifactError> {
    let iter = iter_feature_index(bytes)?;
    let coord_area = iter.coord_area();
    let mut out = Vec::with_capacity(iter.len());
    for entry in iter {
        let entry = entry?;
        let geom = decode_one_geom(coord_area, &entry)?;
        out.push(FeatureGeom {
            id: entry.id,
            bbox: entry.bbox,
            geom,
        });
    }
    Ok(out)
}

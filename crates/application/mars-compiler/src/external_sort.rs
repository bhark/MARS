//! External sort for the per-page flush path.
//!
//! [`external_sort_page`] sorts a page's `Vec<KeyedRow>` by
//! `(hilbert_key, user_id, row_fingerprint)`. Fast path: when the
//! [`MemoryGovernor`] admits the page footprint, the input is sorted in
//! place via `Vec::sort_by` (today's path, byte-identical to the previous
//! behaviour). Slow path: when admission fails the rows are split into
//! chunks small enough to fit, each chunk is sorted in memory and written
//! to a private spill file, then a k-way merge produces the sorted output.
//!
//! On-disk format: one spill run is a leading `u64` row count followed by
//! length-prefixed encoded rows. Encoding is local to this module (no
//! `SpillKind` tag); KeyedRow's geometry, attribute, hilbert key and
//! fingerprint round-trip exactly.
//!
//! Format is process-local and ephemeral. Scratch dir is removed via
//! `TempDir` `Drop` at the end of the function.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mars_artifact::{Coord, FeatureGeom, GeomKind};
use mars_source::AttrValue;
use mars_types::HilbertKey;
use tempfile::TempDir;

use crate::CompilerError;
use crate::memory_governor::MemoryGovernor;
use crate::render::KeyedRow;

const GT_POINT: u8 = 1;
const GT_LINESTRING: u8 = 2;
const GT_POLYGON: u8 = 3;
const GT_MULTIPOINT: u8 = 4;
const GT_MULTILINESTRING: u8 = 5;
const GT_MULTIPOLYGON: u8 = 6;

const AT_NULL: u8 = 0;
const AT_BOOL: u8 = 1;
const AT_INT: u8 = 2;
const AT_FLOAT: u8 = 3;
const AT_STRING: u8 = 4;

/// Comparator shared between the in-memory sort and the k-way merge so
/// the two paths agree byte-for-byte on tie ordering.
fn keyed_row_cmp(a: &KeyedRow, b: &KeyedRow) -> Ordering {
    a.key
        .cmp(&b.key)
        .then_with(|| a.feature.user_id.cmp(&b.feature.user_id))
        .then_with(|| a.row_fingerprint.cmp(&b.row_fingerprint))
}

fn estimate_bytes(rows: &[KeyedRow]) -> u64 {
    rows.iter()
        .map(|r| {
            let attr_bytes: u64 = r.attrs.iter().map(|(k, _)| (k.len() + 16) as u64).sum();
            r.geom_bytes_estimate.saturating_add(attr_bytes).saturating_add(64)
        })
        .sum()
}

fn io_err(what: &'static str, source: std::io::Error) -> CompilerError {
    CompilerError::Spill { what, source }
}

/// Sort `rows` in place by `(hilbert_key, user_id, row_fingerprint)`.
/// Fast path under governor admission; falls back to chunked spill +
/// k-way merge under memory pressure. `chunk_bytes` caps the in-memory
/// footprint of any one chunk under spill (mirrors the per-page
/// working-set ceiling so the existing knob remains a sensible chunk
/// size).
pub(crate) fn external_sort_page(
    rows: Vec<KeyedRow>,
    chunk_bytes: u64,
    scratch_dir: &Path,
    governor: &MemoryGovernor,
) -> Result<Vec<KeyedRow>, CompilerError> {
    let total_bytes = estimate_bytes(&rows);
    // fast path: governor admits the whole page footprint.
    if let Some(_res) = governor.try_acquire(total_bytes) {
        let mut sorted = rows;
        sorted.sort_by(keyed_row_cmp);
        return Ok(sorted);
    }

    // slow path: chunk the input under the governor cap and spill sorted runs.
    let chunk_cap = chunk_bytes.max(1024 * 1024); // never go below 1 MiB to keep merge fan-in sane.
    std::fs::create_dir_all(scratch_dir).map_err(|source| io_err("external_sort: create parent dir", source))?;
    let dir = tempfile::Builder::new()
        .prefix("external-sort-")
        .tempdir_in(scratch_dir)
        .map_err(|source| io_err("external_sort: create scratch dir", source))?;

    let mut runs: Vec<PathBuf> = Vec::new();
    let mut chunk: Vec<KeyedRow> = Vec::new();
    let mut chunk_bytes_acc: u64 = 0;
    for row in rows {
        let row_bytes = estimate_bytes(std::slice::from_ref(&row));
        if !chunk.is_empty() && chunk_bytes_acc.saturating_add(row_bytes) > chunk_cap {
            runs.push(spill_run(&dir, runs.len(), &mut chunk)?);
            chunk_bytes_acc = 0;
        }
        chunk_bytes_acc = chunk_bytes_acc.saturating_add(row_bytes);
        chunk.push(row);
    }
    if !chunk.is_empty() {
        runs.push(spill_run(&dir, runs.len(), &mut chunk)?);
    }

    // k-way merge across all runs into a single Vec.
    kway_merge(&runs, &dir)
}

fn spill_run(dir: &TempDir, run_idx: usize, chunk: &mut Vec<KeyedRow>) -> Result<PathBuf, CompilerError> {
    chunk.sort_by(keyed_row_cmp);
    let path = dir.path().join(format!("run-{run_idx}.bin"));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|source| io_err("external_sort: create run", source))?;
    let mut w = BufWriter::with_capacity(64 * 1024, file);
    let count = chunk.len() as u64;
    w.write_all(&count.to_le_bytes())
        .map_err(|source| io_err("external_sort: write run count", source))?;
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    for row in chunk.drain(..) {
        buf.clear();
        encode_row(&mut buf, &row);
        let len = buf.len() as u32;
        w.write_all(&len.to_le_bytes())
            .map_err(|source| io_err("external_sort: write row length", source))?;
        w.write_all(&buf)
            .map_err(|source| io_err("external_sort: write row body", source))?;
    }
    w.flush().map_err(|source| io_err("external_sort: flush run", source))?;
    Ok(path)
}

struct RunCursor {
    reader: BufReader<File>,
    remaining: u64,
    head: Option<KeyedRow>,
    // ascending tiebreaker so heap order matches keyed_row_cmp insertion order.
    run_id: usize,
}

impl RunCursor {
    fn open(path: &Path, run_id: usize) -> Result<Self, CompilerError> {
        let file = File::open(path).map_err(|source| io_err("external_sort: open run for merge", source))?;
        let mut r = BufReader::with_capacity(64 * 1024, file);
        let mut buf8 = [0u8; 8];
        r.read_exact(&mut buf8)
            .map_err(|source| io_err("external_sort: read run count", source))?;
        let remaining = u64::from_le_bytes(buf8);
        let mut cursor = Self {
            reader: r,
            remaining,
            head: None,
            run_id,
        };
        cursor.advance()?;
        Ok(cursor)
    }

    fn advance(&mut self) -> Result<(), CompilerError> {
        if self.remaining == 0 {
            self.head = None;
            return Ok(());
        }
        let mut len_buf = [0u8; 4];
        self.reader
            .read_exact(&mut len_buf)
            .map_err(|source| io_err("external_sort: read row length", source))?;
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        self.reader
            .read_exact(&mut body)
            .map_err(|source| io_err("external_sort: read row body", source))?;
        self.remaining -= 1;
        self.head = Some(decode_row(&body)?);
        Ok(())
    }
}

// min-heap on (head, run_id). std BinaryHeap is max-heap; wrap in Reverse-via-newtype.
struct HeapEntry {
    key: HilbertKey,
    user_id: u64,
    row_fingerprint: u64,
    run_id: usize,
}

impl HeapEntry {
    fn from_cursor(c: &RunCursor) -> Option<Self> {
        c.head.as_ref().map(|h| Self {
            key: h.key,
            user_id: h.feature.user_id,
            row_fingerprint: h.row_fingerprint,
            run_id: c.run_id,
        })
    }
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for HeapEntry {}
impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // reversed so BinaryHeap behaves as a min-heap.
        other
            .key
            .cmp(&self.key)
            .then_with(|| other.user_id.cmp(&self.user_id))
            .then_with(|| other.row_fingerprint.cmp(&self.row_fingerprint))
            .then_with(|| other.run_id.cmp(&self.run_id))
    }
}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn kway_merge(runs: &[PathBuf], _dir: &TempDir) -> Result<Vec<KeyedRow>, CompilerError> {
    let mut cursors: Vec<RunCursor> = Vec::with_capacity(runs.len());
    for (i, p) in runs.iter().enumerate() {
        cursors.push(RunCursor::open(p, i)?);
    }
    let mut heap: BinaryHeap<HeapEntry> = BinaryHeap::with_capacity(cursors.len());
    for c in &cursors {
        if let Some(e) = HeapEntry::from_cursor(c) {
            heap.push(e);
        }
    }
    let mut out: Vec<KeyedRow> = Vec::new();
    while let Some(top) = heap.pop() {
        let id = top.run_id;
        let row = cursors[id].head.take().ok_or(CompilerError::InvariantViolation {
            what: "external_sort: heap pointed at empty cursor head",
        })?;
        out.push(row);
        cursors[id].advance()?;
        if let Some(e) = HeapEntry::from_cursor(&cursors[id]) {
            heap.push(e);
        }
    }
    Ok(out)
}

// codec ---------------------------------------------------------------

fn encode_row(buf: &mut Vec<u8>, r: &KeyedRow) {
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

struct ByteReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], CompilerError> {
        if self.pos + n > self.bytes.len() {
            return Err(CompilerError::InvariantViolation {
                what: "external_sort: short read",
            });
        }
        let s = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, CompilerError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, CompilerError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn u64(&mut self) -> Result<u64, CompilerError> {
        let s = self.take(8)?;
        Ok(u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
    }
    fn i64(&mut self) -> Result<i64, CompilerError> {
        Ok(self.u64()? as i64)
    }
    fn f32(&mut self) -> Result<f32, CompilerError> {
        let s = self.take(4)?;
        Ok(f32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn f64(&mut self) -> Result<f64, CompilerError> {
        let s = self.take(8)?;
        Ok(f64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
    }
}

fn decode_row(buf: &[u8]) -> Result<KeyedRow, CompilerError> {
    let mut r = ByteReader::new(buf);
    let key = HilbertKey::new(r.u64()?);
    let user_id = r.u64()?;
    let mut bbox = [0f32; 4];
    for c in &mut bbox {
        *c = r.f32()?;
    }
    let geom = decode_geom(&mut r)?;
    let attr_count = r.u32()? as usize;
    let mut attrs: Vec<(String, AttrValue)> = Vec::with_capacity(attr_count);
    for _ in 0..attr_count {
        let nlen = r.u32()? as usize;
        let nb = r.take(nlen)?.to_vec();
        let name = String::from_utf8(nb).map_err(|_| CompilerError::InvariantViolation {
            what: "external_sort: bad utf8 in attr name",
        })?;
        let value = decode_attr(&mut r)?;
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

fn decode_geom(r: &mut ByteReader<'_>) -> Result<GeomKind, CompilerError> {
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
                what: "external_sort: bad geom tag",
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

fn decode_coords(r: &mut ByteReader<'_>) -> Result<Vec<Coord>, CompilerError> {
    let n = r.u32()? as usize;
    let mut out = Vec::with_capacity(n);
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

fn decode_attr(r: &mut ByteReader<'_>) -> Result<AttrValue, CompilerError> {
    Ok(match r.u8()? {
        AT_NULL => AttrValue::Null,
        AT_BOOL => AttrValue::Bool(r.u8()? != 0),
        AT_INT => AttrValue::Int(r.i64()?),
        AT_FLOAT => AttrValue::Float(r.f64()?),
        AT_STRING => {
            let n = r.u32()? as usize;
            let sb = r.take(n)?.to_vec();
            let s = String::from_utf8(sb).map_err(|_| CompilerError::InvariantViolation {
                what: "external_sort: bad utf8 in attr string",
            })?;
            AttrValue::String(s)
        }
        _ => {
            return Err(CompilerError::InvariantViolation {
                what: "external_sort: bad attr tag",
            });
        }
    })
}

#[inline]
fn u32_try(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests;

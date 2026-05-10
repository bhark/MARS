//! Pass-2 disk-spill fallback for [`crate::render::rebuild_binding_from_plan`].
//!
//! When the in-memory partial-page footprint crosses the configured soft
//! threshold, the compiler hands all current partial buffers to a
//! [`SpillManager`] which writes them to per-page files under a
//! per-binding scratch dir. Subsequent rows for an already-spilled page
//! are appended directly to its file. On page completion, the file is
//! drained back into a `Vec<KeyedRow>` and passed to the unchanged
//! [`crate::render::flush_one_page`] path. The dir is removed on drop.
//!
//! Format: per file, a 4-byte magic `b"MSPL"` and a u16 version, followed
//! by length-implicit row records. Each record is a 1-byte kind tag
//! (0 = kept, 1 = pruned) followed by a hand-rolled binary `KeyedRow`
//! encoding. The format is process-local and ephemeral; no checksum, no
//! cross-version stability.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hashlink::LinkedHashMap;
use mars_artifact::{Coord, FeatureGeom, GeomKind};
use mars_source::AttrValue;
use mars_types::{HilbertKey, PageId};
use tempfile::TempDir;

use crate::CompilerError;
use crate::render::KeyedRow;

const MAGIC: &[u8; 4] = b"MSPL";
const FORMAT_VERSION: u16 = 1;

const KIND_KEPT: u8 = 0;
const KIND_PRUNED: u8 = 1;

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

/// Tag for whether a spilled row was kept (passed `geometry_min_size_m`)
/// or pruned (failed) during pass-2 routing.
#[derive(Debug, Clone, Copy)]
pub(crate) enum SpillKind {
    Kept,
    Pruned,
}

impl SpillKind {
    fn tag(self) -> u8 {
        match self {
            Self::Kept => KIND_KEPT,
            Self::Pruned => KIND_PRUNED,
        }
    }
}

/// Snapshot of spill-side counters captured at end-of-binding for
/// observability.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SpillMetrics {
    pub bytes_written: u64,
    pub bytes_read: u64,
    pub files_active_peak: usize,
    pub triggered: bool,
}

/// Per-binding spill state. Owns a temporary directory whose lifetime is
/// the lifetime of this struct (RAII cleanup on drop).
pub(crate) struct SpillManager {
    dir: TempDir,
    open_files: LinkedHashMap<(usize, PageId), BufWriter<File>>,
    spilled: std::collections::HashSet<(usize, PageId)>,
    file_limit: usize,
    bytes_written: u64,
    bytes_read: u64,
    files_active_peak: usize,
    triggered: bool,
}

impl SpillManager {
    /// Create a fresh per-binding spill subdir under `parent_dir`. The
    /// parent must exist and be writable.
    pub(crate) fn new(parent_dir: &Path, file_limit: usize) -> Result<Self, CompilerError> {
        let limit = file_limit.max(1);
        std::fs::create_dir_all(parent_dir).map_err(|source| CompilerError::Spill {
            what: "create parent dir",
            source,
        })?;
        let dir = tempfile::Builder::new()
            .prefix("binding-")
            .tempdir_in(parent_dir)
            .map_err(|source| CompilerError::Spill {
                what: "create binding tempdir",
                source,
            })?;
        Ok(Self {
            dir,
            open_files: LinkedHashMap::new(),
            spilled: std::collections::HashSet::new(),
            file_limit: limit,
            bytes_written: 0,
            bytes_read: 0,
            files_active_peak: 0,
            triggered: false,
        })
    }

    pub(crate) fn is_spilled(&self, lvl: usize, page_id: PageId) -> bool {
        self.spilled.contains(&(lvl, page_id))
    }

    pub(crate) fn metrics(&self) -> SpillMetrics {
        SpillMetrics {
            bytes_written: self.bytes_written,
            bytes_read: self.bytes_read,
            files_active_peak: self.files_active_peak,
            triggered: self.triggered,
        }
    }

    /// Append one row to the spill file for `(lvl, page_id)`. Marks the
    /// page as spilled. Opens (and may evict from the LRU) on miss.
    /// Returns the encoded byte length written.
    pub(crate) fn append(
        &mut self,
        lvl: usize,
        page_id: PageId,
        kind: SpillKind,
        row: &KeyedRow,
    ) -> Result<u64, CompilerError> {
        let mut buf: Vec<u8> = Vec::with_capacity(128);
        encode_row(&mut buf, kind, row);
        let n = buf.len() as u64;
        let writer = self.ensure_open(lvl, page_id)?;
        writer.write_all(&buf).map_err(|source| CompilerError::Spill {
            what: "append row",
            source,
        })?;
        self.bytes_written = self.bytes_written.saturating_add(n);
        Ok(n)
    }

    /// Spill every entry in `partial` and `pruned` to disk and clear them.
    /// Returns the total in-memory byte estimate that was evicted, summed
    /// from `page_bytes` so the caller can keep its `in_flight_bytes`
    /// running total accurate.
    pub(crate) fn flush_all_partials(
        &mut self,
        partial: &mut std::collections::HashMap<(usize, PageId), Vec<KeyedRow>>,
        pruned: &mut std::collections::HashMap<(usize, PageId), Vec<KeyedRow>>,
        page_bytes: &mut std::collections::HashMap<(usize, PageId), u64>,
    ) -> Result<u64, CompilerError> {
        self.triggered = true;
        let mut evicted: u64 = 0;
        // collect keys first so we can mutate partial / pruned while iterating.
        let mut keys: Vec<(usize, PageId)> = partial.keys().copied().collect();
        for k in pruned.keys().copied() {
            if !partial.contains_key(&k) {
                keys.push(k);
            }
        }
        for k in keys {
            if let Some(rows) = partial.remove(&k) {
                for r in &rows {
                    self.append(k.0, k.1, SpillKind::Kept, r)?;
                }
            }
            if let Some(rows) = pruned.remove(&k) {
                for r in &rows {
                    self.append(k.0, k.1, SpillKind::Pruned, r)?;
                }
            }
            if let Some(b) = page_bytes.remove(&k) {
                evicted = evicted.saturating_add(b);
            }
        }
        Ok(evicted)
    }

    /// Drain the spill file for `(lvl, page_id)` into `(kept, pruned)`
    /// vectors. Closes the file and removes it from disk. Caller is
    /// responsible for appending any remaining in-memory tail before
    /// passing to `flush_one_page`.
    pub(crate) fn drain(
        &mut self,
        lvl: usize,
        page_id: PageId,
    ) -> Result<(Vec<KeyedRow>, Vec<KeyedRow>), CompilerError> {
        if let Some(mut w) = self.open_files.remove(&(lvl, page_id)) {
            w.flush().map_err(|source| CompilerError::Spill {
                what: "flush before drain",
                source,
            })?;
        }
        let path = self.path_for(lvl, page_id);
        let f = File::open(&path).map_err(|source| CompilerError::Spill {
            what: "open spill for drain",
            source,
        })?;
        let mut reader = BufReader::new(f);
        let header_len = read_header(&mut reader)?;
        let mut total_read = u64::from(header_len);
        let mut kept: Vec<KeyedRow> = Vec::new();
        let mut pruned: Vec<KeyedRow> = Vec::new();
        while let Some((kind, row, n)) = read_row(&mut reader)? {
            total_read = total_read.saturating_add(n);
            match kind {
                SpillKind::Kept => kept.push(row),
                SpillKind::Pruned => pruned.push(row),
            }
        }
        drop(reader);
        std::fs::remove_file(&path).map_err(|source| CompilerError::Spill {
            what: "remove spill after drain",
            source,
        })?;
        self.spilled.remove(&(lvl, page_id));
        self.bytes_read = self.bytes_read.saturating_add(total_read);
        Ok((kept, pruned))
    }

    fn ensure_open(&mut self, lvl: usize, page_id: PageId) -> Result<&mut BufWriter<File>, CompilerError> {
        let key = (lvl, page_id);
        if self.open_files.contains_key(&key) {
            // refresh LRU position.
            return self.open_files.to_back(&key).ok_or(CompilerError::InvariantViolation {
                what: "spill: get_refresh after contains_key",
            });
        }
        while self.open_files.len() >= self.file_limit {
            if let Some((_, mut w)) = self.open_files.pop_front() {
                w.flush().map_err(|source| CompilerError::Spill {
                    what: "flush on lru evict",
                    source,
                })?;
            } else {
                break;
            }
        }
        let path = self.path_for(lvl, page_id);
        let is_new = !self.spilled.contains(&key);
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| CompilerError::Spill {
                what: "open spill file",
                source,
            })?;
        let mut w = BufWriter::with_capacity(64 * 1024, f);
        if is_new {
            write_header(&mut w)?;
            self.bytes_written = self.bytes_written.saturating_add(u64::from(header_len()));
            self.spilled.insert(key);
        }
        self.open_files.insert(key, w);
        if self.open_files.len() > self.files_active_peak {
            self.files_active_peak = self.open_files.len();
        }
        // safe: we just inserted.
        self.open_files.to_back(&key).ok_or(CompilerError::InvariantViolation {
            what: "spill: get_refresh after insert",
        })
    }

    fn path_for(&self, lvl: usize, page_id: PageId) -> PathBuf {
        self.dir.path().join(format!("p-l{}-p{}.spill", lvl, page_id.get()))
    }
}

// codec ---------------------------------------------------------------

fn header_len() -> u32 {
    (MAGIC.len() + std::mem::size_of::<u16>()) as u32
}

fn write_header(w: &mut BufWriter<File>) -> Result<(), CompilerError> {
    w.write_all(MAGIC).map_err(|source| CompilerError::Spill {
        what: "write magic",
        source,
    })?;
    w.write_all(&FORMAT_VERSION.to_le_bytes())
        .map_err(|source| CompilerError::Spill {
            what: "write version",
            source,
        })?;
    Ok(())
}

fn read_header(r: &mut BufReader<File>) -> Result<u32, CompilerError> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic).map_err(|source| CompilerError::Spill {
        what: "read magic",
        source,
    })?;
    if &magic != MAGIC {
        return Err(CompilerError::InvariantViolation {
            what: "spill: bad magic",
        });
    }
    let mut ver = [0u8; 2];
    r.read_exact(&mut ver).map_err(|source| CompilerError::Spill {
        what: "read version",
        source,
    })?;
    let v = u16::from_le_bytes(ver);
    if v != FORMAT_VERSION {
        return Err(CompilerError::InvariantViolation {
            what: "spill: bad version",
        });
    }
    Ok(header_len())
}

fn encode_row(buf: &mut Vec<u8>, kind: SpillKind, r: &KeyedRow) {
    buf.push(kind.tag());
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

fn read_row(r: &mut BufReader<File>) -> Result<Option<(SpillKind, KeyedRow, u64)>, CompilerError> {
    let mut tag = [0u8; 1];
    match r.read(&mut tag).map_err(|source| CompilerError::Spill {
        what: "read kind tag",
        source,
    })? {
        0 => return Ok(None),
        1 => {}
        _ => unreachable!("read returns 0 or 1 with single-byte buffer"),
    }
    let kind = match tag[0] {
        KIND_KEPT => SpillKind::Kept,
        KIND_PRUNED => SpillKind::Pruned,
        _ => {
            return Err(CompilerError::InvariantViolation {
                what: "spill: bad kind tag",
            });
        }
    };
    let mut started = 1u64;
    let key = HilbertKey::new(read_u64(r, &mut started)?);
    let user_id = read_u64(r, &mut started)?;
    let mut bbox = [0f32; 4];
    for c in &mut bbox {
        *c = read_f32(r, &mut started)?;
    }
    let geom = read_geom(r, &mut started)?;
    let attr_count = read_u32(r, &mut started)? as usize;
    let mut attrs: Vec<(String, AttrValue)> = Vec::with_capacity(attr_count);
    for _ in 0..attr_count {
        let nlen = read_u32(r, &mut started)? as usize;
        let mut nb = vec![0u8; nlen];
        r.read_exact(&mut nb).map_err(|source| CompilerError::Spill {
            what: "read attr name",
            source,
        })?;
        started = started.saturating_add(nlen as u64);
        let name = String::from_utf8(nb).map_err(|_| CompilerError::InvariantViolation {
            what: "spill: bad utf8 in attr name",
        })?;
        let value = read_attr(r, &mut started)?;
        attrs.push((name, value));
    }
    let geom_bytes_estimate = read_u64(r, &mut started)?;
    let row_fingerprint = read_u64(r, &mut started)?;

    let row = KeyedRow {
        feature: FeatureGeom { user_id, bbox, geom },
        attrs: Arc::new(attrs),
        geom_bytes_estimate,
        key,
        row_fingerprint,
    };
    Ok(Some((kind, row, started)))
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

fn read_geom(r: &mut BufReader<File>, n: &mut u64) -> Result<GeomKind, CompilerError> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag).map_err(|source| CompilerError::Spill {
        what: "read geom tag",
        source,
    })?;
    *n = n.saturating_add(1);
    Ok(match tag[0] {
        GT_POINT => {
            let x = read_f64(r, n)?;
            let y = read_f64(r, n)?;
            GeomKind::Point((x, y))
        }
        GT_LINESTRING => GeomKind::LineString(read_coords(r, n)?),
        GT_POLYGON => {
            let rings = read_u32(r, n)? as usize;
            let mut out = Vec::with_capacity(rings);
            for _ in 0..rings {
                out.push(read_coords(r, n)?);
            }
            GeomKind::Polygon(out)
        }
        GT_MULTIPOINT => GeomKind::MultiPoint(read_coords(r, n)?),
        GT_MULTILINESTRING => {
            let parts = read_u32(r, n)? as usize;
            let mut out = Vec::with_capacity(parts);
            for _ in 0..parts {
                out.push(read_coords(r, n)?);
            }
            GeomKind::MultiLineString(out)
        }
        GT_MULTIPOLYGON => {
            let parts = read_u32(r, n)? as usize;
            let mut out = Vec::with_capacity(parts);
            for _ in 0..parts {
                let rings = read_u32(r, n)? as usize;
                let mut poly = Vec::with_capacity(rings);
                for _ in 0..rings {
                    poly.push(read_coords(r, n)?);
                }
                out.push(poly);
            }
            GeomKind::MultiPolygon(out)
        }
        _ => {
            return Err(CompilerError::InvariantViolation {
                what: "spill: bad geom tag",
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

fn read_coords(r: &mut BufReader<File>, n: &mut u64) -> Result<Vec<Coord>, CompilerError> {
    let count = read_u32(r, n)? as usize;
    let mut out: Vec<Coord> = Vec::with_capacity(count);
    for _ in 0..count {
        let x = read_f64(r, n)?;
        let y = read_f64(r, n)?;
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

fn read_attr(r: &mut BufReader<File>, n: &mut u64) -> Result<AttrValue, CompilerError> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag).map_err(|source| CompilerError::Spill {
        what: "read attr tag",
        source,
    })?;
    *n = n.saturating_add(1);
    Ok(match tag[0] {
        AT_NULL => AttrValue::Null,
        AT_BOOL => {
            let mut b = [0u8; 1];
            r.read_exact(&mut b).map_err(|source| CompilerError::Spill {
                what: "read bool",
                source,
            })?;
            *n = n.saturating_add(1);
            AttrValue::Bool(b[0] != 0)
        }
        AT_INT => AttrValue::Int(read_i64(r, n)?),
        AT_FLOAT => AttrValue::Float(read_f64(r, n)?),
        AT_STRING => {
            let len = read_u32(r, n)? as usize;
            let mut sb = vec![0u8; len];
            r.read_exact(&mut sb).map_err(|source| CompilerError::Spill {
                what: "read string body",
                source,
            })?;
            *n = n.saturating_add(len as u64);
            let s = String::from_utf8(sb).map_err(|_| CompilerError::InvariantViolation {
                what: "spill: bad utf8 in attr string",
            })?;
            AttrValue::String(s)
        }
        _ => {
            return Err(CompilerError::InvariantViolation {
                what: "spill: bad attr tag",
            });
        }
    })
}

#[inline]
fn u32_try(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn read_u32(r: &mut BufReader<File>, n: &mut u64) -> Result<u32, CompilerError> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).map_err(|source| CompilerError::Spill {
        what: "read u32",
        source,
    })?;
    *n = n.saturating_add(4);
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut BufReader<File>, n: &mut u64) -> Result<u64, CompilerError> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b).map_err(|source| CompilerError::Spill {
        what: "read u64",
        source,
    })?;
    *n = n.saturating_add(8);
    Ok(u64::from_le_bytes(b))
}

fn read_i64(r: &mut BufReader<File>, n: &mut u64) -> Result<i64, CompilerError> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b).map_err(|source| CompilerError::Spill {
        what: "read i64",
        source,
    })?;
    *n = n.saturating_add(8);
    Ok(i64::from_le_bytes(b))
}

fn read_f32(r: &mut BufReader<File>, n: &mut u64) -> Result<f32, CompilerError> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).map_err(|source| CompilerError::Spill {
        what: "read f32",
        source,
    })?;
    *n = n.saturating_add(4);
    Ok(f32::from_le_bytes(b))
}

fn read_f64(r: &mut BufReader<File>, n: &mut u64) -> Result<f64, CompilerError> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b).map_err(|source| CompilerError::Spill {
        what: "read f64",
        source,
    })?;
    *n = n.saturating_add(8);
    Ok(f64::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use std::collections::HashMap;

    use mars_artifact::{FeatureGeom, GeomKind};
    use mars_source::AttrValue;
    use mars_types::{HilbertKey, PageId};
    use tempfile::TempDir;

    use super::*;

    fn sample_row(seed: u64) -> KeyedRow {
        KeyedRow {
            feature: FeatureGeom {
                user_id: seed,
                bbox: [seed as f32, seed as f32 + 1.0, seed as f32 + 2.0, seed as f32 + 3.0],
                geom: GeomKind::Polygon(vec![vec![(1.0, 2.0), (3.0, 4.0), (5.0, 6.0), (1.0, 2.0)]]),
            },
            attrs: Arc::new(vec![
                ("name".into(), AttrValue::String(format!("row-{seed}"))),
                ("count".into(), AttrValue::Int(seed as i64 * 2)),
                ("ratio".into(), AttrValue::Float(seed as f64 / 3.0)),
                ("flag".into(), AttrValue::Bool(seed.is_multiple_of(2))),
                ("missing".into(), AttrValue::Null),
            ]),
            geom_bytes_estimate: 256 + seed,
            key: HilbertKey::new(seed.wrapping_mul(7919)),
            row_fingerprint: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15),
        }
    }

    fn rows_eq(a: &KeyedRow, b: &KeyedRow) -> bool {
        a.feature == b.feature
            && a.geom_bytes_estimate == b.geom_bytes_estimate
            && a.key == b.key
            && a.row_fingerprint == b.row_fingerprint
            && a.attrs.as_slice() == b.attrs.as_slice()
    }

    #[test]
    fn roundtrip_kept_and_pruned() {
        let parent = TempDir::new().unwrap();
        let mut spill = SpillManager::new(parent.path(), 32).unwrap();
        let r1 = sample_row(1);
        let r2 = sample_row(2);
        let r3 = sample_row(3);
        spill.append(0, PageId::new(7), SpillKind::Kept, &r1).unwrap();
        spill.append(0, PageId::new(7), SpillKind::Pruned, &r2).unwrap();
        spill.append(0, PageId::new(7), SpillKind::Kept, &r3).unwrap();
        assert!(spill.is_spilled(0, PageId::new(7)));
        let (kept, pruned) = spill.drain(0, PageId::new(7)).unwrap();
        assert_eq!(kept.len(), 2);
        assert_eq!(pruned.len(), 1);
        assert!(rows_eq(&kept[0], &r1));
        assert!(rows_eq(&kept[1], &r3));
        assert!(rows_eq(&pruned[0], &r2));
        assert!(!spill.is_spilled(0, PageId::new(7)));
    }

    #[test]
    fn lru_eviction_reopens_in_append_mode() {
        let parent = TempDir::new().unwrap();
        let mut spill = SpillManager::new(parent.path(), 2).unwrap();
        // populate three pages; LRU = 2 forces an eviction.
        spill
            .append(0, PageId::new(1), SpillKind::Kept, &sample_row(10))
            .unwrap();
        spill
            .append(0, PageId::new(2), SpillKind::Kept, &sample_row(20))
            .unwrap();
        spill
            .append(0, PageId::new(3), SpillKind::Kept, &sample_row(30))
            .unwrap();
        // touch page 1 again; it was evicted, must reopen and append without
        // overwriting the header.
        spill
            .append(0, PageId::new(1), SpillKind::Kept, &sample_row(11))
            .unwrap();
        let (kept, _) = spill.drain(0, PageId::new(1)).unwrap();
        assert_eq!(kept.len(), 2);
        assert!(rows_eq(&kept[0], &sample_row(10)));
        assert!(rows_eq(&kept[1], &sample_row(11)));
    }

    #[test]
    fn drop_removes_dir() {
        let parent = TempDir::new().unwrap();
        let dir_path = {
            let mut spill = SpillManager::new(parent.path(), 4).unwrap();
            spill
                .append(0, PageId::new(0), SpillKind::Kept, &sample_row(1))
                .unwrap();
            spill.dir.path().to_path_buf()
        };
        assert!(!dir_path.exists(), "binding tempdir should be removed on drop");
    }

    #[test]
    fn flush_all_partials_evicts_and_clears_maps() {
        let parent = TempDir::new().unwrap();
        let mut spill = SpillManager::new(parent.path(), 16).unwrap();
        let mut partial: HashMap<(usize, PageId), Vec<KeyedRow>> = HashMap::new();
        let mut pruned: HashMap<(usize, PageId), Vec<KeyedRow>> = HashMap::new();
        let mut page_bytes: HashMap<(usize, PageId), u64> = HashMap::new();
        partial.insert((0, PageId::new(0)), vec![sample_row(1), sample_row(2)]);
        partial.insert((0, PageId::new(1)), vec![sample_row(3)]);
        pruned.insert((0, PageId::new(0)), vec![sample_row(99)]);
        page_bytes.insert((0, PageId::new(0)), 1000);
        page_bytes.insert((0, PageId::new(1)), 500);
        let evicted = spill
            .flush_all_partials(&mut partial, &mut pruned, &mut page_bytes)
            .unwrap();
        assert_eq!(evicted, 1500);
        assert!(partial.is_empty());
        assert!(pruned.is_empty());
        assert!(page_bytes.is_empty());
        assert!(spill.metrics().triggered);
        let (kept0, pruned0) = spill.drain(0, PageId::new(0)).unwrap();
        assert_eq!(kept0.len(), 2);
        assert_eq!(pruned0.len(), 1);
        let (kept1, pruned1) = spill.drain(0, PageId::new(1)).unwrap();
        assert_eq!(kept1.len(), 1);
        assert!(pruned1.is_empty());
    }
}

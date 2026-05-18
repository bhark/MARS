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
//! length-prefixed `KeyedRow` bodies encoded via [`crate::scratch_codec`].
//! No `SpillKind` tag (this path keeps the kept-only stream).
//!
//! Format is process-local and ephemeral. Scratch dir is removed via
//! `TempDir` `Drop` at the end of the function.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use mars_types::HilbertKey;
use tempfile::TempDir;

use crate::CompilerError;
use crate::memory_governor::MemoryGovernor;
use crate::render::KeyedRow;
use crate::scratch_codec::{ScratchReader, decode_keyed_row_body, encode_keyed_row_body};

// floor on the per-chunk byte budget. anything below this would push merge
// fan-in past sensible bounds for the page sizes we see in practice.
const EXTERNAL_SORT_MIN_CHUNK_BYTES: u64 = 1024 * 1024;

// BufReader/BufWriter capacity for run files. small enough to keep RAM
// flat under high merge fan-in, large enough to amortise syscalls.
const EXTERNAL_SORT_BUF_BYTES: usize = 64 * 1024;

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
    let chunk_cap = chunk_bytes.max(EXTERNAL_SORT_MIN_CHUNK_BYTES);
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
    let mut w = BufWriter::with_capacity(EXTERNAL_SORT_BUF_BYTES, file);
    let count = chunk.len() as u64;
    w.write_all(&count.to_le_bytes())
        .map_err(|source| io_err("external_sort: write run count", source))?;
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    for row in chunk.drain(..) {
        buf.clear();
        encode_keyed_row_body(&mut buf, &row);
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
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
        let mut r = BufReader::with_capacity(EXTERNAL_SORT_BUF_BYTES, file);
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
        let mut br = ByteReader::new(&body);
        self.head = Some(decode_keyed_row_body(&mut br)?);
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

// ScratchReader adapter over a length-prefixed row body buffer.
struct ByteReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
}

impl ScratchReader for ByteReader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], CompilerError> {
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

#[cfg(test)]
mod tests;

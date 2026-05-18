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
//! (0 = kept, 1 = pruned) followed by a `KeyedRow` body encoded via
//! [`crate::scratch_codec`]. The format is process-local and ephemeral;
//! no checksum, no cross-version stability.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use hashlink::LinkedHashMap;
use mars_types::PageId;
use tempfile::TempDir;

use crate::CompilerError;
use crate::disk_governor::{DiskGovernor, DiskReservation};
use crate::render::KeyedRow;
use crate::scratch_codec::{ScratchReader, decode_keyed_row_body, encode_keyed_row_body};

const MAGIC: &[u8; 4] = b"MSPL";
const FORMAT_VERSION: u16 = 1;

const KIND_KEPT: u8 = 0;
const KIND_PRUNED: u8 = 1;

// per-file write-side BufWriter capacity. enough to amortise small-row
// append syscalls without hoarding RAM per open page.
const SPILL_WRITE_BUF_BYTES: usize = 64 * 1024;

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
    // disk-governor reservations indexed by page; drained on `drain` and
    // cleared on drop so every byte the spill file holds is admitted and
    // released through the shared governor.
    reservations: HashMap<(usize, PageId), Vec<DiskReservation>>,
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
            reservations: HashMap::new(),
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
    ///
    /// Admission: the encoded row's byte length is acquired against
    /// `disk_governor` before the write hits the BufWriter. Header bytes
    /// for newly-opened files are admitted inside `ensure_open`. Both
    /// reservations are attached to the per-page entry in `reservations`
    /// and released when `drain` removes the file or when the manager is
    /// dropped at end-of-binding.
    pub(crate) async fn append(
        &mut self,
        lvl: usize,
        page_id: PageId,
        kind: SpillKind,
        row: &KeyedRow,
        disk_governor: &DiskGovernor,
    ) -> Result<u64, CompilerError> {
        let mut buf: Vec<u8> = Vec::with_capacity(128);
        encode_row(&mut buf, kind, row);
        let n = buf.len() as u64;
        // header bytes (if a new file) are admitted inside ensure_open
        // before the row reservation, so a tight budget that can hold one
        // row but not row+header cannot deadlock by holding row bytes
        // while waiting on header bytes.
        self.ensure_open(lvl, page_id, disk_governor).await?;
        let row_reservation = disk_governor
            .acquire(n)
            .await
            .map_err(|source| CompilerError::DiskGovernor { source })?;
        let writer = self
            .open_files
            .get_mut(&(lvl, page_id))
            .ok_or(CompilerError::InvariantViolation {
                what: "spill: writer vanished after ensure_open",
            })?;
        writer.write_all(&buf).map_err(|source| CompilerError::Spill {
            what: "append row",
            source,
        })?;
        self.bytes_written = self.bytes_written.saturating_add(n);
        self.reservations
            .entry((lvl, page_id))
            .or_default()
            .push(row_reservation);
        Ok(n)
    }

    /// Spill every entry in `partial` and `pruned` to disk and clear them.
    /// Returns the total in-memory byte estimate that was evicted, summed
    /// from `page_bytes` so the caller can keep its `in_flight_bytes`
    /// running total accurate.
    pub(crate) async fn flush_all_partials(
        &mut self,
        partial: &mut HashMap<(usize, PageId), Vec<KeyedRow>>,
        pruned: &mut HashMap<(usize, PageId), Vec<KeyedRow>>,
        page_bytes: &mut HashMap<(usize, PageId), u64>,
        disk_governor: &DiskGovernor,
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
                    self.append(k.0, k.1, SpillKind::Kept, r, disk_governor).await?;
                }
            }
            if let Some(rows) = pruned.remove(&k) {
                for r in &rows {
                    self.append(k.0, k.1, SpillKind::Pruned, r, disk_governor).await?;
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
        // release the page's disk-governor reservations: the file is gone,
        // so the bytes it admitted are no longer in flight.
        self.reservations.remove(&(lvl, page_id));
        self.bytes_read = self.bytes_read.saturating_add(total_read);
        Ok((kept, pruned))
    }

    async fn ensure_open(
        &mut self,
        lvl: usize,
        page_id: PageId,
        disk_governor: &DiskGovernor,
    ) -> Result<(), CompilerError> {
        let key = (lvl, page_id);
        if self.open_files.contains_key(&key) {
            // refresh LRU position.
            let _ = self.open_files.to_back(&key).ok_or(CompilerError::InvariantViolation {
                what: "spill: get_refresh after contains_key",
            })?;
            return Ok(());
        }
        self.evict_oldest_until_under_limit()?;
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
        let mut w = BufWriter::with_capacity(SPILL_WRITE_BUF_BYTES, f);
        if is_new {
            // admit header bytes before they hit disk. acquired here (no
            // row reservation held yet) so a tight budget can not deadlock
            // on a row+header sequence.
            let header_reservation = disk_governor
                .acquire(u64::from(header_len()))
                .await
                .map_err(|source| CompilerError::DiskGovernor { source })?;
            write_header(&mut w)?;
            self.bytes_written = self.bytes_written.saturating_add(u64::from(header_len()));
            self.spilled.insert(key);
            self.reservations.entry(key).or_default().push(header_reservation);
        }
        self.open_files.insert(key, w);
        if self.open_files.len() > self.files_active_peak {
            self.files_active_peak = self.open_files.len();
        }
        Ok(())
    }

    fn evict_oldest_until_under_limit(&mut self) -> Result<(), CompilerError> {
        while self.open_files.len() >= self.file_limit {
            let Some((_, mut w)) = self.open_files.pop_front() else {
                break;
            };
            w.flush().map_err(|source| CompilerError::Spill {
                what: "flush on lru evict",
                source,
            })?;
        }
        Ok(())
    }

    fn path_for(&self, lvl: usize, page_id: PageId) -> PathBuf {
        self.dir.path().join(format!("p-l{}-p{}.spill", lvl, page_id.get()))
    }
}

// framing -------------------------------------------------------------

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
    encode_keyed_row_body(buf, r);
}

fn read_row(r: &mut BufReader<File>) -> Result<Option<(SpillKind, KeyedRow, u64)>, CompilerError> {
    // single-byte non-`read_exact` so a clean EOF on the row boundary
    // yields Ok(None) instead of an error.
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
    let mut reader = CountingReader::new(r);
    let row = decode_keyed_row_body(&mut reader)?;
    // 1 byte kind tag + everything the codec consumed.
    let n = 1u64.saturating_add(reader.bytes_read);
    Ok(Some((kind, row, n)))
}

// ScratchReader adapter for spill: BufReader<File> plus a running byte
// counter so `drain` can report how much of the spill file it consumed.
struct CountingReader<'a> {
    inner: &'a mut BufReader<File>,
    bytes_read: u64,
    scratch: Vec<u8>,
}

impl<'a> CountingReader<'a> {
    fn new(inner: &'a mut BufReader<File>) -> Self {
        Self {
            inner,
            bytes_read: 0,
            scratch: Vec::new(),
        }
    }
}

impl ScratchReader for CountingReader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], CompilerError> {
        self.scratch.resize(n, 0);
        self.inner
            .read_exact(&mut self.scratch)
            .map_err(|source| CompilerError::Spill {
                what: "scratch_codec: take",
                source,
            })?;
        self.bytes_read = self.bytes_read.saturating_add(n as u64);
        Ok(&self.scratch)
    }
    fn u8(&mut self) -> Result<u8, CompilerError> {
        let mut b = [0u8; 1];
        self.inner.read_exact(&mut b).map_err(|source| CompilerError::Spill {
            what: "scratch_codec: read u8",
            source,
        })?;
        self.bytes_read = self.bytes_read.saturating_add(1);
        Ok(b[0])
    }
    fn u32(&mut self) -> Result<u32, CompilerError> {
        let mut b = [0u8; 4];
        self.inner.read_exact(&mut b).map_err(|source| CompilerError::Spill {
            what: "scratch_codec: read u32",
            source,
        })?;
        self.bytes_read = self.bytes_read.saturating_add(4);
        Ok(u32::from_le_bytes(b))
    }
    fn u64(&mut self) -> Result<u64, CompilerError> {
        let mut b = [0u8; 8];
        self.inner.read_exact(&mut b).map_err(|source| CompilerError::Spill {
            what: "scratch_codec: read u64",
            source,
        })?;
        self.bytes_read = self.bytes_read.saturating_add(8);
        Ok(u64::from_le_bytes(b))
    }
    fn i64(&mut self) -> Result<i64, CompilerError> {
        Ok(self.u64()? as i64)
    }
    fn f32(&mut self) -> Result<f32, CompilerError> {
        let mut b = [0u8; 4];
        self.inner.read_exact(&mut b).map_err(|source| CompilerError::Spill {
            what: "scratch_codec: read f32",
            source,
        })?;
        self.bytes_read = self.bytes_read.saturating_add(4);
        Ok(f32::from_le_bytes(b))
    }
    fn f64(&mut self) -> Result<f64, CompilerError> {
        let mut b = [0u8; 8];
        self.inner.read_exact(&mut b).map_err(|source| CompilerError::Spill {
            what: "scratch_codec: read f64",
            source,
        })?;
        self.bytes_read = self.bytes_read.saturating_add(8);
        Ok(f64::from_le_bytes(b))
    }
}

#[cfg(test)]
mod tests;

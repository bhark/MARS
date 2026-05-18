//! Pass-2 row-key → route lookup as a sorted index walked beside a sorted stream.
//!
//! Pass 2 must answer "which `(level, page_id)` targets does this row
//! belong to?" once per streamed row. The plan-time set of routes is
//! built up front; the row stream emits keys in ascending byte order
//! (single-worker heap scan, BE-encoded `(tableoid, block, offset)`),
//! so the route table can be modeled as a sorted index walked alongside
//! the stream rather than a random-access map.
//!
//! Build phase: [`RouteIndex`] gates inserts on a [`MemoryGovernor`]
//! reservation. As long as the governor admits, entries land in an
//! in-memory `BTreeMap`. When admission fails the current map is sorted
//! and flushed to a per-run on-disk file, the in-memory map starts
//! fresh, and inserts continue. A small `MIN_SPILL_BATCH_BYTES` floor
//! prevents the 1-entry-per-spill-file pathology under hostile budgets:
//! below the floor an admission failure is informational, not action-
//! triggering.
//!
//! Freeze phase: [`RouteIndex::freeze`] streams a k-way merge across
//! the in-memory map and every spilled run into a single sorted
//! `frozen.idx` file, then returns a [`RouteCursor`] over it. Per-key
//! collisions across sources concatenate route lists in source order.
//!
//! Lookup phase: [`RouteCursor::advance_to`] walks the merged file with
//! a single `BufReader`, advancing past keys that don't match the
//! caller's monotonic input. Total disk I/O per binding is bounded and
//! sequential; no per-row syscalls beyond the BufReader fill amortizes
//! over.
//!
//! Per-record file layout (used identically for spilled runs and the
//! frozen file): `u64 count_le` then `count × (16 B key, 1 B n_routes,
//! n_routes × (4 B level_le, 8 B page_id_le))`, sorted by lexicographic
//! key. Format is process-local and ephemeral; no checksum, no cross-
//! version stability. Scratch dir is removed via `TempDir` `Drop` at
//! session end.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use mars_source::SourceRowKey;
use mars_types::PageId;
use tempfile::TempDir;

use crate::CompilerError;
use crate::memory_governor::{MemoryGovernor, MemoryReservation};

pub(crate) type RouteList = Vec<(usize, PageId)>;

const KEY_BYTES: usize = 16;
const ROUTE_BYTES: u64 = 4 + 8; // u32 lvl + u64 page
// rough heap accounting: BTreeMap node + Vec header + key copy.
const ENTRY_OVERHEAD: u64 = 64;
// floor below which a failed governor admission does not trigger a spill.
// keeps run files at a useful minimum size even when the cap is so tight
// that no admission ever succeeds.
const MIN_SPILL_BATCH_BYTES: u64 = 16 * 1024 * 1024;
// BufReader / BufWriter capacity for run + frozen file I/O.
const IO_BUF_BYTES: usize = 64 * 1024;

pub(crate) struct RouteIndex {
    in_mem: BTreeMap<SourceRowKey, RouteList>,
    in_mem_bytes: u64,
    reservation: Option<MemoryReservation>,
    governor: MemoryGovernor,
    runs: Vec<PathBuf>,
    dir: TempDir,
}

/// Stats surfaced from [`RouteIndex::freeze`] for tracing.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FreezeStats {
    pub entries_total: u64,
    pub runs_merged: u64,
    pub bytes_written: u64,
    pub elapsed_ms: u64,
}

impl RouteIndex {
    pub(crate) fn with_governor(governor: &MemoryGovernor, parent_dir: &Path) -> Result<Self, CompilerError> {
        std::fs::create_dir_all(parent_dir).map_err(|source| CompilerError::Spill {
            what: "route_index: create parent dir",
            source,
        })?;
        let dir = tempfile::Builder::new()
            .prefix("route-index-")
            .tempdir_in(parent_dir)
            .map_err(|source| CompilerError::Spill {
                what: "route_index: create scratch dir",
                source,
            })?;
        Ok(Self {
            in_mem: BTreeMap::new(),
            in_mem_bytes: 0,
            reservation: None,
            governor: governor.clone(),
            runs: Vec::new(),
            dir,
        })
    }

    /// Insert a single `(key, route)` pair. Multiple inserts for the same
    /// key accumulate routes; the order matches `Vec::push`.
    pub(crate) fn insert(&mut self, key: SourceRowKey, route: (usize, PageId)) -> Result<(), CompilerError> {
        let added = if self.in_mem.contains_key(&key) {
            ROUTE_BYTES
        } else {
            ROUTE_BYTES + KEY_BYTES as u64 + ENTRY_OVERHEAD
        };
        if !self.try_grow(added) {
            // hostile-budget guard: only spill once we've accumulated a
            // meaningful chunk. otherwise the map stays small and unaccounted
            // until either the cap relaxes or the floor is exceeded.
            if self.in_mem_bytes >= MIN_SPILL_BATCH_BYTES && !self.in_mem.is_empty() {
                self.spill_to_run()?;
                let after_added = ROUTE_BYTES + KEY_BYTES as u64 + ENTRY_OVERHEAD;
                let _ = self.try_grow(after_added);
            }
            // below the floor, accept unaccounted.
        }
        self.in_mem.entry(key).or_default().push(route);
        Ok(())
    }

    /// Drain build state into a single sorted file and return a forward
    /// cursor over it. Consumes `self`; on error the scratch dir is
    /// dropped along with `self`.
    pub(crate) fn freeze(mut self) -> Result<(RouteCursor, FreezeStats), CompilerError> {
        let started = std::time::Instant::now();
        // flush the residual in-memory map as a final run so the merge below
        // is a single uniform pass over file-backed cursors. an empty residual
        // produces no run.
        if !self.in_mem.is_empty() {
            self.spill_to_run()?;
        }

        let runs = std::mem::take(&mut self.runs);
        let runs_merged = runs.len() as u64;

        let frozen_path = self.dir.path().join("frozen.idx");
        let frozen_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&frozen_path)
            .map_err(|source| CompilerError::Spill {
                what: "route_index: create frozen file",
                source,
            })?;

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

        // streaming merge into BufWriter; emit one record per unique key,
        // concatenating routes when multiple sources hold the same key.
        let mut writer = BufWriter::with_capacity(
            IO_BUF_BYTES,
            frozen_file.try_clone().map_err(|source| CompilerError::Spill {
                what: "route_index: clone frozen file handle",
                source,
            })?,
        );
        // count is patched once entries_total is known.
        writer
            .write_all(&0u64.to_le_bytes())
            .map_err(|source| CompilerError::Spill {
                what: "route_index: write frozen header",
                source,
            })?;
        let mut entries_total: u64 = 0;
        let mut bytes_written: u64 = 8; // header

        let mut current_key: Option<[u8; KEY_BYTES]> = None;
        let mut current_routes: RouteList = Vec::new();

        while let Some(top) = heap.pop() {
            let id = top.run_id;
            let head = cursors[id].head.take().ok_or(CompilerError::InvariantViolation {
                what: "route_index: heap pointed at empty cursor head",
            })?;
            let (key, routes) = head;
            match current_key {
                Some(k) if k == key => {
                    if current_routes.len() + routes.len() > usize::from(u8::MAX) {
                        return Err(CompilerError::InvariantViolation {
                            what: "route_index: more than 255 routes for a single row_key",
                        });
                    }
                    current_routes.extend(routes);
                }
                Some(prev) => {
                    bytes_written = bytes_written.saturating_add(write_record(&mut writer, &prev, &current_routes)?);
                    entries_total = entries_total.saturating_add(1);
                    current_key = Some(key);
                    current_routes = routes;
                }
                None => {
                    current_key = Some(key);
                    current_routes = routes;
                }
            }
            cursors[id].advance()?;
            if let Some(e) = HeapEntry::from_cursor(&cursors[id]) {
                heap.push(e);
            }
        }
        if let Some(k) = current_key {
            bytes_written = bytes_written.saturating_add(write_record(&mut writer, &k, &current_routes)?);
            entries_total = entries_total.saturating_add(1);
        }
        writer.flush().map_err(|source| CompilerError::Spill {
            what: "route_index: flush frozen file",
            source,
        })?;
        // patch count header.
        let mut frozen_writeback = writer.into_inner().map_err(|e| CompilerError::Spill {
            what: "route_index: into_inner frozen writer",
            source: e.into_error(),
        })?;
        use std::io::{Seek, SeekFrom};
        frozen_writeback
            .seek(SeekFrom::Start(0))
            .map_err(|source| CompilerError::Spill {
                what: "route_index: seek frozen header",
                source,
            })?;
        frozen_writeback
            .write_all(&entries_total.to_le_bytes())
            .map_err(|source| CompilerError::Spill {
                what: "route_index: patch frozen header",
                source,
            })?;
        frozen_writeback.flush().map_err(|source| CompilerError::Spill {
            what: "route_index: flush patched header",
            source,
        })?;

        // drop per-run cursors (closes file handles); also remove the run
        // files since the merged file supersedes them. tempdir drop at
        // session end cleans up anything we miss.
        drop(cursors);
        for p in &runs {
            let _ = std::fs::remove_file(p);
        }

        // release the build-phase governor reservation; the cursor's working
        // set is bounded by IO_BUF_BYTES + the peek buffer, well below any
        // accounting threshold worth re-acquiring for.
        self.reservation = None;
        self.in_mem_bytes = 0;

        let cursor_file = OpenOptions::new()
            .read(true)
            .open(&frozen_path)
            .map_err(|source| CompilerError::Spill {
                what: "route_index: reopen frozen file",
                source,
            })?;
        let mut reader = BufReader::with_capacity(IO_BUF_BYTES, cursor_file);
        let mut count_buf = [0u8; 8];
        reader
            .read_exact(&mut count_buf)
            .map_err(|source| CompilerError::Spill {
                what: "route_index: read frozen header",
                source,
            })?;
        let count = u64::from_le_bytes(count_buf);
        debug_assert_eq!(count, entries_total);

        let stats = FreezeStats {
            entries_total,
            runs_merged,
            bytes_written,
            elapsed_ms: started.elapsed().as_millis() as u64,
        };
        Ok((
            RouteCursor {
                reader,
                _dir: self.dir,
                peek: None,
                last_seen_key: None,
                entries_remaining: count,
            },
            stats,
        ))
    }

    fn try_grow(&mut self, additional: u64) -> bool {
        let want = self.in_mem_bytes.saturating_add(additional);
        // tokio's semaphore has no split; release the prior reservation and
        // re-acquire the enlarged amount in one step. since the governor is
        // a counter, ordering is irrelevant.
        self.reservation = None;
        match self.governor.try_acquire(want) {
            Some(r) => {
                self.reservation = Some(r);
                self.in_mem_bytes = want;
                true
            }
            None => {
                // re-acquire what we held before so peak stays accounted.
                if self.in_mem_bytes > 0 {
                    self.reservation = self.governor.try_acquire(self.in_mem_bytes);
                }
                false
            }
        }
    }

    fn spill_to_run(&mut self) -> Result<(), CompilerError> {
        if self.in_mem.is_empty() {
            return Ok(());
        }
        let path = self.dir.path().join(format!("run-{}.idx", self.runs.len()));
        let map = std::mem::take(&mut self.in_mem);
        let n_entries = map.len() as u64;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|source| CompilerError::Spill {
                what: "route_index: create run file",
                source,
            })?;
        let mut w = BufWriter::with_capacity(IO_BUF_BYTES, file);
        w.write_all(&n_entries.to_le_bytes())
            .map_err(|source| CompilerError::Spill {
                what: "route_index: write run count",
                source,
            })?;
        for (key, routes) in &map {
            if routes.len() > usize::from(u8::MAX) {
                return Err(CompilerError::InvariantViolation {
                    what: "route_index: more than 255 routes for a single row_key",
                });
            }
            write_record(&mut w, key.as_bytes(), routes)?;
        }
        w.flush().map_err(|source| CompilerError::Spill {
            what: "route_index: flush run",
            source,
        })?;
        self.runs.push(path);
        // released alongside the cleared map.
        self.reservation = None;
        self.in_mem_bytes = 0;
        Ok(())
    }
}

fn write_record<W: Write>(w: &mut W, key: &[u8; KEY_BYTES], routes: &RouteList) -> Result<u64, CompilerError> {
    if routes.len() > usize::from(u8::MAX) {
        return Err(CompilerError::InvariantViolation {
            what: "route_index: more than 255 routes for a single row_key",
        });
    }
    w.write_all(key).map_err(io_err)?;
    let n = routes.len() as u8;
    w.write_all(std::slice::from_ref(&n)).map_err(io_err)?;
    for (lvl, page) in routes {
        let lvl32 = u32::try_from(*lvl).map_err(|_| CompilerError::InvariantViolation {
            what: "route_index: level index exceeds u32",
        })?;
        w.write_all(&lvl32.to_le_bytes()).map_err(io_err)?;
        w.write_all(&page.0.to_le_bytes()).map_err(io_err)?;
    }
    Ok(KEY_BYTES as u64 + 1 + routes.len() as u64 * ROUTE_BYTES)
}

fn io_err(source: std::io::Error) -> CompilerError {
    CompilerError::Spill {
        what: "route_index: io",
        source,
    }
}

/// Forward-only cursor over the merged sorted file produced by
/// [`RouteIndex::freeze`]. Lookups must arrive in monotonic key order.
pub(crate) struct RouteCursor {
    reader: BufReader<File>,
    _dir: TempDir,
    peek: Option<([u8; KEY_BYTES], RouteList)>,
    last_seen_key: Option<[u8; KEY_BYTES]>,
    entries_remaining: u64,
}

impl RouteCursor {
    /// Walk forward to the entry matching `key`. Returns `Some(routes)`
    /// when the cursor's current entry matches, `None` when `key` is
    /// not indexed (the caller skips this row). Calls must be monotonic
    /// in `key`; a regression returns `InvariantViolation`.
    pub(crate) fn advance_to(&mut self, key: &SourceRowKey) -> Result<Option<RouteList>, CompilerError> {
        let target = key.as_bytes();
        if let Some(prev) = &self.last_seen_key
            && target.as_slice() < prev.as_slice()
        {
            return Err(CompilerError::InvariantViolation {
                what: "route_index: cursor advance_to received a non-monotonic key",
            });
        }
        self.last_seen_key = Some(*target);

        loop {
            let Some((peek_key, _)) = &self.peek else {
                if !self.fill_peek()? {
                    return Ok(None);
                }
                continue;
            };
            match peek_key.as_slice().cmp(target.as_slice()) {
                Ordering::Less => {
                    // discard and continue.
                    self.peek = None;
                }
                Ordering::Equal => {
                    return Ok(self.peek.take().map(|(_, routes)| routes));
                }
                Ordering::Greater => return Ok(None),
            }
        }
    }

    fn fill_peek(&mut self) -> Result<bool, CompilerError> {
        if self.entries_remaining == 0 {
            return Ok(false);
        }
        let mut key_buf = [0u8; KEY_BYTES];
        self.reader.read_exact(&mut key_buf).map_err(io_err)?;
        let mut n_buf = [0u8; 1];
        self.reader.read_exact(&mut n_buf).map_err(io_err)?;
        let n = n_buf[0] as usize;
        let mut routes: RouteList = Vec::with_capacity(n);
        for _ in 0..n {
            let mut lvl_buf = [0u8; 4];
            self.reader.read_exact(&mut lvl_buf).map_err(io_err)?;
            let mut page_buf = [0u8; 8];
            self.reader.read_exact(&mut page_buf).map_err(io_err)?;
            routes.push((
                u32::from_le_bytes(lvl_buf) as usize,
                PageId(u64::from_le_bytes(page_buf)),
            ));
        }
        self.entries_remaining -= 1;
        self.peek = Some((key_buf, routes));
        Ok(true)
    }
}

// k-way merge plumbing -------------------------------------------------

struct RunCursor {
    reader: BufReader<File>,
    remaining: u64,
    head: Option<([u8; KEY_BYTES], RouteList)>,
    run_id: usize,
}

impl RunCursor {
    fn open(path: &Path, run_id: usize) -> Result<Self, CompilerError> {
        let file = File::open(path).map_err(|source| CompilerError::Spill {
            what: "route_index: open run for merge",
            source,
        })?;
        let mut r = BufReader::with_capacity(IO_BUF_BYTES, file);
        let mut count_buf = [0u8; 8];
        r.read_exact(&mut count_buf).map_err(|source| CompilerError::Spill {
            what: "route_index: read run count",
            source,
        })?;
        let remaining = u64::from_le_bytes(count_buf);
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
        let mut key_buf = [0u8; KEY_BYTES];
        self.reader.read_exact(&mut key_buf).map_err(io_err)?;
        let mut n_buf = [0u8; 1];
        self.reader.read_exact(&mut n_buf).map_err(io_err)?;
        let n = n_buf[0] as usize;
        let mut routes: RouteList = Vec::with_capacity(n);
        for _ in 0..n {
            let mut lvl_buf = [0u8; 4];
            self.reader.read_exact(&mut lvl_buf).map_err(io_err)?;
            let mut page_buf = [0u8; 8];
            self.reader.read_exact(&mut page_buf).map_err(io_err)?;
            routes.push((
                u32::from_le_bytes(lvl_buf) as usize,
                PageId(u64::from_le_bytes(page_buf)),
            ));
        }
        self.remaining -= 1;
        self.head = Some((key_buf, routes));
        Ok(())
    }
}

struct HeapEntry {
    key: [u8; KEY_BYTES],
    run_id: usize,
}

impl HeapEntry {
    fn from_cursor(c: &RunCursor) -> Option<Self> {
        c.head.as_ref().map(|h| Self {
            key: h.0,
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
        // reversed so BinaryHeap behaves as a min-heap. tiebreak on run_id
        // ascending so equal-key entries pop in source order; the freeze
        // pass concatenates routes in that order to keep the merged result
        // deterministic.
        other
            .key
            .as_slice()
            .cmp(self.key.as_slice())
            .then_with(|| other.run_id.cmp(&self.run_id))
    }
}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests;

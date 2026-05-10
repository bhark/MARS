//! Pass-2 row-key → route lookup, governor-bounded with on-disk spill.
//!
//! Pass 2 must answer "which `(level, page_id)` targets does this row
//! belong to?" once per streamed row. The plan-time set of routes is
//! built up front, so today's `HashMap<SourceRowKey, RouteList>` peaks
//! at ~50 bytes × `total_planned` rows — ~300 MiB on a 6 M-row binding,
//! held resident for the whole pass-2 lifetime.
//!
//! [`RouteIndex`] gates inserts on a [`MemoryGovernor`] reservation. As
//! long as the governor admits, entries land in an in-memory `BTreeMap`.
//! When admission fails the current map is sorted and flushed to a
//! per-run on-disk file under a private scratch dir, the in-memory map
//! starts fresh, and inserts continue. Lookups consult the in-memory map
//! plus every spilled run, merging the route lists; a key that straddles
//! runs is correctly recombined.
//!
//! File layout per run:
//! - header section: `n_entries` × 24 B = 16 B `SourceRowKey` + 4 B
//!   payload offset + 4 B payload length, sorted by key (lexicographic
//!   on the raw 16 bytes, matching the `Ord` impl for `[u8; 16]`).
//! - payload section: variable-length `(u8 n_routes, n × (u32 lvl, u64
//!   page_id))` records, indexed by the offsets in the header.
//!
//! Format is process-local and ephemeral; no checksum, no cross-version
//! stability. Scratch dir is removed via `TempDir` `Drop` at session end.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use mars_source::SourceRowKey;
use mars_types::PageId;
use tempfile::TempDir;

use crate::CompilerError;
use crate::memory_governor::{MemoryGovernor, MemoryReservation};

pub(crate) type RouteList = Vec<(usize, PageId)>;

const KEY_BYTES: u64 = 16;
const HEADER_ENTRY_BYTES: u64 = KEY_BYTES + 4 + 4; // 24
const ROUTE_BYTES: u64 = 4 + 8; // u32 lvl + u64 page
// rough heap accounting: BTreeMap node + Vec header + key copy.
const ENTRY_OVERHEAD: u64 = 64;

pub(crate) struct RouteIndex {
    in_mem: BTreeMap<SourceRowKey, RouteList>,
    in_mem_bytes: u64,
    reservation: Option<MemoryReservation>,
    governor: MemoryGovernor,
    runs: Vec<RouteRun>,
    dir: TempDir,
}

struct RouteRun {
    file: File,
    header_count: u64,
    payload_offset: u64,
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
            ROUTE_BYTES + KEY_BYTES + ENTRY_OVERHEAD
        };
        if !self.try_grow(added) {
            self.spill_to_run()?;
            // post-spill the in-mem map is empty, so this insert pays the
            // full per-key overhead. failure here means the governor is
            // saturated by other consumers; proceed unaccounted.
            let after_added = ROUTE_BYTES + KEY_BYTES + ENTRY_OVERHEAD;
            let _ = self.try_grow(after_added);
        }
        self.in_mem.entry(key).or_default().push(route);
        Ok(())
    }

    /// Lookup all routes for `key`, merging across the in-memory map and
    /// every spilled run. Returns `None` only when no run knows the key.
    pub(crate) fn lookup(&mut self, key: &SourceRowKey) -> Result<Option<RouteList>, CompilerError> {
        let mut combined: RouteList = self.in_mem.get(key).cloned().unwrap_or_default();
        for run in &mut self.runs {
            if let Some(r) = run.lookup(key)? {
                combined.extend(r);
            }
        }
        if combined.is_empty() {
            Ok(None)
        } else {
            Ok(Some(combined))
        }
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
        let header_bytes_total = n_entries * HEADER_ENTRY_BYTES;
        let mut header: Vec<u8> = Vec::with_capacity(header_bytes_total as usize);
        let mut payload: Vec<u8> = Vec::new();
        for (key, routes) in &map {
            let off: u32 = u32::try_from(payload.len()).map_err(|_| CompilerError::InvariantViolation {
                what: "route_index: payload offset exceeded u32",
            })?;
            // payload too long to address with u32 means a single key is
            // mapped to >300M routes — far beyond any plausible plan.
            let trimmed: &[(usize, PageId)] = if routes.len() > usize::from(u8::MAX) {
                return Err(CompilerError::InvariantViolation {
                    what: "route_index: more than 255 routes for a single row_key",
                });
            } else {
                routes.as_slice()
            };
            payload.push(trimmed.len() as u8);
            for (lvl, page) in trimmed {
                let lvl32 = u32::try_from(*lvl).map_err(|_| CompilerError::InvariantViolation {
                    what: "route_index: level index exceeds u32",
                })?;
                payload.extend_from_slice(&lvl32.to_le_bytes());
                payload.extend_from_slice(&page.0.to_le_bytes());
            }
            let len: u32 = u32::try_from(payload.len()).map_err(|_| CompilerError::InvariantViolation {
                what: "route_index: payload length exceeded u32",
            })? - off;
            header.extend_from_slice(key.as_bytes());
            header.extend_from_slice(&off.to_le_bytes());
            header.extend_from_slice(&len.to_le_bytes());
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|source| CompilerError::Spill {
                what: "route_index: create run file",
                source,
            })?;
        file.write_all(&header).map_err(|source| CompilerError::Spill {
            what: "route_index: write header",
            source,
        })?;
        file.write_all(&payload).map_err(|source| CompilerError::Spill {
            what: "route_index: write payload",
            source,
        })?;
        self.runs.push(RouteRun {
            file,
            header_count: n_entries,
            payload_offset: header_bytes_total,
        });
        // released alongside the cleared map.
        self.reservation = None;
        self.in_mem_bytes = 0;
        Ok(())
    }
}

impl RouteRun {
    fn lookup(&mut self, key: &SourceRowKey) -> Result<Option<RouteList>, CompilerError> {
        let n = self.header_count;
        if n == 0 {
            return Ok(None);
        }
        let mut lo: u64 = 0;
        let mut hi: u64 = n;
        let target = key.as_bytes();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let off = mid * HEADER_ENTRY_BYTES;
            self.file.seek(SeekFrom::Start(off)).map_err(io_err)?;
            let mut buf = [0u8; 24];
            self.file.read_exact(&mut buf).map_err(io_err)?;
            let mid_key = &buf[..16];
            match mid_key.cmp(&target[..]) {
                std::cmp::Ordering::Equal => {
                    let payload_off = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]) as u64;
                    let payload_len = u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]) as usize;
                    let mut pbuf = vec![0u8; payload_len];
                    self.file
                        .seek(SeekFrom::Start(self.payload_offset + payload_off))
                        .map_err(io_err)?;
                    self.file.read_exact(&mut pbuf).map_err(io_err)?;
                    return Ok(Some(decode_payload(&pbuf)?));
                }
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        Ok(None)
    }
}

fn decode_payload(buf: &[u8]) -> Result<RouteList, CompilerError> {
    if buf.is_empty() {
        return Err(CompilerError::InvariantViolation {
            what: "route_index: empty payload",
        });
    }
    let n = buf[0] as usize;
    let need = 1 + n * (ROUTE_BYTES as usize);
    if buf.len() < need {
        return Err(CompilerError::InvariantViolation {
            what: "route_index: truncated payload",
        });
    }
    let mut out = Vec::with_capacity(n);
    let mut p = 1usize;
    for _ in 0..n {
        let lvl = u32::from_le_bytes([buf[p], buf[p + 1], buf[p + 2], buf[p + 3]]) as usize;
        let page = u64::from_le_bytes([
            buf[p + 4],
            buf[p + 5],
            buf[p + 6],
            buf[p + 7],
            buf[p + 8],
            buf[p + 9],
            buf[p + 10],
            buf[p + 11],
        ]);
        out.push((lvl, PageId(page)));
        p += ROUTE_BYTES as usize;
    }
    Ok(out)
}

fn io_err(source: std::io::Error) -> CompilerError {
    CompilerError::Spill {
        what: "route_index: io",
        source,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SourceRowKey {
        SourceRowKey::from_bytes([seed; 16])
    }

    #[test]
    fn in_memory_round_trip() {
        let g = MemoryGovernor::new(64 * 1024 * 1024);
        let mut idx = RouteIndex::with_governor(&g, std::env::temp_dir().as_path()).unwrap();
        idx.insert(key(1), (0, PageId(10))).unwrap();
        idx.insert(key(1), (1, PageId(20))).unwrap();
        idx.insert(key(2), (0, PageId(30))).unwrap();
        let r = idx.lookup(&key(1)).unwrap().unwrap();
        assert_eq!(r, vec![(0, PageId(10)), (1, PageId(20))]);
        let r = idx.lookup(&key(2)).unwrap().unwrap();
        assert_eq!(r, vec![(0, PageId(30))]);
        assert!(idx.lookup(&key(3)).unwrap().is_none());
    }

    #[test]
    fn spilled_round_trip_merges_runs() {
        // tight cap forces a spill on every other insert.
        let g = MemoryGovernor::new(96);
        let mut idx = RouteIndex::with_governor(&g, std::env::temp_dir().as_path()).unwrap();
        for i in 0..16u8 {
            idx.insert(key(i), (i as usize, PageId(u64::from(i)))).unwrap();
        }
        assert!(!idx.runs.is_empty(), "expected spill activity under tight cap");
        for i in 0..16u8 {
            let r = idx.lookup(&key(i)).unwrap().unwrap();
            assert_eq!(r, vec![(i as usize, PageId(u64::from(i)))]);
        }
        assert!(idx.lookup(&key(99)).unwrap().is_none());
    }

    #[test]
    fn key_straddling_runs_recombines() {
        let g = MemoryGovernor::new(96);
        let mut idx = RouteIndex::with_governor(&g, std::env::temp_dir().as_path()).unwrap();
        // first batch of inserts for key(7) lands before the spill.
        idx.insert(key(7), (0, PageId(1))).unwrap();
        // force spills by jamming many other keys through.
        for i in 10..40u8 {
            idx.insert(key(i), (0, PageId(u64::from(i)))).unwrap();
        }
        // additional route for the same key now lands in a newer run.
        idx.insert(key(7), (1, PageId(99))).unwrap();
        let r = idx.lookup(&key(7)).unwrap().unwrap();
        assert!(r.contains(&(0, PageId(1))));
        assert!(r.contains(&(1, PageId(99))));
        assert_eq!(r.len(), 2);
    }
}

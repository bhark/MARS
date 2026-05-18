//! Append-only on-disk arena for pass-1 sidecar entries.
//!
//! Pass 1 records `(user_id, hilbert_key)` for every unfiltered row; pass
//! 2 hands the same set to [`crate::sidecar::encode_sidecar`]. Holding the
//! list in a `Vec` peaks at 16 bytes × `feature_count_total` (~96 MiB on
//! a 6 M-row binding). The arena avoids a second transient copy by writing
//! directly to disk in pass 1 and reading sequentially in pass 2.
//!
//! This arena writes each pair as a fixed 16-byte record into a private
//! scratch file during pass 1 and exposes a sequential reader during pass
//! 2. Ownership of the scratch directory is shared via `Arc<TempDir>` so
//! the file outlives the writer; cleanup is RAII.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mars_types::HilbertKey;
use tempfile::TempDir;

use crate::CompilerError;

const RECORD_BYTES: usize = 16; // 8 user_id + 8 hilbert

fn io_err(what: &'static str, source: std::io::Error) -> CompilerError {
    CompilerError::Spill { what, source }
}

/// Write side of the arena. Buffered and append-only.
pub struct SidecarArenaWriter {
    file: BufWriter<File>,
    path: PathBuf,
    dir: Arc<TempDir>,
    count: u64,
}

impl SidecarArenaWriter {
    /// Allocate a fresh arena under `parent_dir`. Parent must exist.
    pub fn new(parent_dir: &Path) -> Result<Self, CompilerError> {
        std::fs::create_dir_all(parent_dir).map_err(|source| io_err("sidecar_arena: create parent dir", source))?;
        let dir = tempfile::Builder::new()
            .prefix("sidecar-arena-")
            .tempdir_in(parent_dir)
            .map_err(|source| io_err("sidecar_arena: create scratch dir", source))?;
        let path = dir.path().join("entries.bin");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|source| io_err("sidecar_arena: create file", source))?;
        Ok(Self {
            file: BufWriter::new(file),
            path,
            dir: Arc::new(dir),
            count: 0,
        })
    }

    /// Append one entry. Order matches `Vec::push`.
    pub fn push(&mut self, user_id: u64, key: HilbertKey) -> Result<(), CompilerError> {
        let mut buf = [0u8; RECORD_BYTES];
        buf[..8].copy_from_slice(&user_id.to_le_bytes());
        buf[8..].copy_from_slice(&key.get().to_le_bytes());
        self.file
            .write_all(&buf)
            .map_err(|source| io_err("sidecar_arena: write record", source))?;
        self.count = self.count.saturating_add(1);
        Ok(())
    }

    /// Finalise into a read-only arena. Flushes the buffer and hands the
    /// scratch dir's `Arc` to the arena so the file outlives this writer.
    pub fn finish(mut self) -> Result<SidecarArena, CompilerError> {
        self.file
            .flush()
            .map_err(|source| io_err("sidecar_arena: flush", source))?;
        Ok(SidecarArena {
            path: self.path,
            _dir: self.dir,
            count: self.count,
        })
    }
}

/// Read side of the arena. Sequentially iterable; cheap to clone (shared
/// scratch dir via `Arc`).
#[derive(Clone)]
pub struct SidecarArena {
    path: PathBuf,
    // RAII: keeps the scratch dir alive as long as any arena clone exists.
    _dir: Arc<TempDir>,
    count: u64,
}

impl std::fmt::Debug for SidecarArena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidecarArena")
            .field("count", &self.count)
            .field("path", &self.path)
            .finish()
    }
}

impl PartialEq for SidecarArena {
    fn eq(&self, other: &Self) -> bool {
        // arenas are anonymous on-disk scratch; identity is path-based and
        // not meaningfully comparable across instances. matches the way
        // PagePlan equality is used in tests (rebuild fixtures construct
        // their own plans, not compare across runs).
        self.path == other.path && self.count == other.count
    }
}

impl SidecarArena {
    /// Number of records appended.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.count
    }

    /// True if no records were appended.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Drain the arena into an owned `Vec`. Used by pass 2 to feed
    /// [`crate::sidecar::encode_sidecar`], which sorts and encodes. The
    /// arena's record set is held only once at peak (no clone).
    pub fn drain_into_vec(&self) -> Result<Vec<(u64, HilbertKey)>, CompilerError> {
        let file = File::open(&self.path).map_err(|source| io_err("sidecar_arena: open file", source))?;
        let mut r = BufReader::new(file);
        let n = usize::try_from(self.count).map_err(|_| CompilerError::InvariantViolation {
            what: "sidecar_arena: count exceeds usize",
        })?;
        let mut out: Vec<(u64, HilbertKey)> = Vec::with_capacity(n);
        let mut buf = [0u8; RECORD_BYTES];
        for _ in 0..n {
            r.read_exact(&mut buf)
                .map_err(|source| io_err("sidecar_arena: read record", source))?;
            let id = u64::from_le_bytes([buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7]]);
            let key = u64::from_le_bytes([buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15]]);
            out.push((id, HilbertKey::new(key)));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;

//! page-membership sidecar codec.
//!
//! per binding, a flat mmap-friendly binary file pairing every source
//! `user_id` with its hilbert key. user_id is allowed to repeat - a source
//! row exploded into multiple parts contributes one entry per part, all
//! sharing the same user_id but landing on (potentially) different hilbert
//! keys. used by the incremental compile path in C.2 to resolve
//! change-feed events to dirty page sets without round-tripping through
//! cells; written atomically alongside the manifest in the snapshot path
//! so the runtime / GFI can rely on the manifest's
//! `BindingMetadata::page_membership_sidecar` reference.
//!
//! on-disk format (little-endian):
//! ```text
//!   [magic "PMSC" u32][version u32 = 2][count u64]
//!   entries: count × [u64 user_id][u64 hilbert_key]
//!     // sorted by (user_id ascending, hilbert_key ascending);
//!     // user_id may repeat
//! ```
//! 16 bytes per entry -> ~800 MiB at 50M entries (production-class).

use bytes::Bytes;
use mars_types::HilbertKey;

const MAGIC: u32 = 0x_434D_5350; // "PMSC" little-endian
const VERSION: u32 = 1;
const HEADER_LEN: usize = 4 + 4 + 8;
const ENTRY_LEN: usize = 16;

/// Errors produced while encoding or decoding a page-membership sidecar.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SidecarError {
    #[error("sidecar: bad magic or version")]
    BadHeader,
    #[error("sidecar: truncated buffer")]
    Truncated,
    #[error("sidecar: declared count {declared} does not match buffer size")]
    BadCount {
        /// count declared in the header
        declared: u64,
    },
    #[error("sidecar: entries not sorted ascending by (user_id, hilbert_key)")]
    Unsorted,
    #[error("sidecar: count {0} exceeds u64 byte budget")]
    CountTooLarge(usize),
}

/// Encode a page-membership sidecar from `(user_id, hilbert_key)` pairs.
/// The slice is sorted in place by `(user_id, hilbert_key)`; user_id is
/// permitted to repeat (multimap semantics - a source row exploded into N
/// parts contributes N entries with the same user_id).
pub fn encode_sidecar(entries: &mut [(u64, HilbertKey)]) -> Result<Bytes, SidecarError> {
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let count = entries.len();
    let total_len = HEADER_LEN
        .checked_add(count.checked_mul(ENTRY_LEN).ok_or(SidecarError::CountTooLarge(count))?)
        .ok_or(SidecarError::CountTooLarge(count))?;

    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(count as u64).to_le_bytes());
    for (id, key) in entries {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&key.get().to_le_bytes());
    }
    Ok(Bytes::from(out))
}

/// Read-only view over a page-membership sidecar payload.
#[derive(Debug, Clone, Copy)]
pub struct SidecarReader<'a> {
    bytes: &'a [u8],
    count: usize,
}

impl<'a> SidecarReader<'a> {
    /// Validate the header and the entry count; defer per-entry parsing to
    /// [`Self::lookup`]. ascending order is verified up front so binary
    /// search is meaningful and forged blobs cannot drive a panic.
    pub fn open(bytes: &'a [u8]) -> Result<Self, SidecarError> {
        if bytes.len() < HEADER_LEN {
            return Err(SidecarError::Truncated);
        }
        let magic = u32::from_le_bytes(bytes[0..4].try_into().map_err(|_| SidecarError::BadHeader)?);
        let version = u32::from_le_bytes(bytes[4..8].try_into().map_err(|_| SidecarError::BadHeader)?);
        if magic != MAGIC || version != VERSION {
            return Err(SidecarError::BadHeader);
        }
        let declared = u64::from_le_bytes(bytes[8..16].try_into().map_err(|_| SidecarError::BadHeader)?);
        let count: usize = declared.try_into().map_err(|_| SidecarError::BadCount { declared })?;
        let body_len = bytes.len() - HEADER_LEN;
        if count.checked_mul(ENTRY_LEN) != Some(body_len) {
            return Err(SidecarError::BadCount { declared });
        }

        // verify ascending (user_id, hilbert_key) order. duplicates of
        // (user_id, hilbert_key) are tolerated (a row may land on the same
        // hilbert key as itself across multimap entries); the multimap
        // semantics rely on user_ids forming contiguous runs.
        let mut prev: Option<(u64, u64)> = None;
        for i in 0..count {
            let off = HEADER_LEN + i * ENTRY_LEN;
            let id = u64::from_le_bytes(bytes[off..off + 8].try_into().map_err(|_| SidecarError::Truncated)?);
            let key = u64::from_le_bytes(
                bytes[off + 8..off + 16]
                    .try_into()
                    .map_err(|_| SidecarError::Truncated)?,
            );
            if let Some(p) = prev
                && (id, key) < p
            {
                return Err(SidecarError::Unsorted);
            }
            prev = Some((id, key));
        }

        Ok(Self { bytes, count })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.count
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Iterate every `(feature_id, hilbert_key)` pair in feature_id order.
    /// O(N); intended for incremental rebuilds that need to scan the whole
    /// binding once per cycle.
    pub fn iter(&self) -> impl Iterator<Item = (u64, HilbertKey)> + '_ {
        (0..self.count).map(move |i| {
            let off = HEADER_LEN + i * ENTRY_LEN;
            let id = u64::from_le_bytes([
                self.bytes[off],
                self.bytes[off + 1],
                self.bytes[off + 2],
                self.bytes[off + 3],
                self.bytes[off + 4],
                self.bytes[off + 5],
                self.bytes[off + 6],
                self.bytes[off + 7],
            ]);
            let kbase = off + 8;
            let key = u64::from_le_bytes([
                self.bytes[kbase],
                self.bytes[kbase + 1],
                self.bytes[kbase + 2],
                self.bytes[kbase + 3],
                self.bytes[kbase + 4],
                self.bytes[kbase + 5],
                self.bytes[kbase + 6],
                self.bytes[kbase + 7],
            ]);
            (id, HilbertKey::new(key))
        })
    }

    /// Collect every `user_id` whose hilbert key falls in any of `ranges`.
    /// Each range is `(lo, hi)` inclusive. Single-pass over the sidecar; the
    /// returned vec may contain repeats when multiple multimap entries for
    /// the same user_id land in the dirty range. Used by the rebuild path
    /// to resolve dirty-page member sets.
    #[must_use]
    pub fn user_ids_in_ranges(&self, ranges: &[(HilbertKey, HilbertKey)]) -> Vec<u64> {
        if ranges.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<u64> = Vec::new();
        for (id, key) in self.iter() {
            if ranges.iter().any(|(lo, hi)| key >= *lo && key <= *hi) {
                out.push(id);
            }
        }
        out
    }

    /// Yield every `hilbert_key` recorded for `user_id` in the sidecar.
    /// Returns an empty iterator when the id is absent. Multimap semantics:
    /// a source row exploded into N parts produces N keys here.
    pub fn lookup_all(&self, user_id: u64) -> impl Iterator<Item = HilbertKey> + '_ {
        let start = self.lower_bound(user_id);
        SidecarRangeIter {
            sidecar: self,
            cursor: start,
            target: user_id,
        }
    }

    fn read_id_at(&self, slot: usize) -> u64 {
        let off = HEADER_LEN + slot * ENTRY_LEN;
        u64::from_le_bytes([
            self.bytes[off],
            self.bytes[off + 1],
            self.bytes[off + 2],
            self.bytes[off + 3],
            self.bytes[off + 4],
            self.bytes[off + 5],
            self.bytes[off + 6],
            self.bytes[off + 7],
        ])
    }

    fn read_key_at(&self, slot: usize) -> HilbertKey {
        let off = HEADER_LEN + slot * ENTRY_LEN + 8;
        HilbertKey::new(u64::from_le_bytes([
            self.bytes[off],
            self.bytes[off + 1],
            self.bytes[off + 2],
            self.bytes[off + 3],
            self.bytes[off + 4],
            self.bytes[off + 5],
            self.bytes[off + 6],
            self.bytes[off + 7],
        ]))
    }

    fn lower_bound(&self, target: u64) -> usize {
        let mut lo = 0usize;
        let mut hi = self.count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.read_id_at(mid) < target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

struct SidecarRangeIter<'a> {
    sidecar: &'a SidecarReader<'a>,
    cursor: usize,
    target: u64,
}

impl<'a> Iterator for SidecarRangeIter<'a> {
    type Item = HilbertKey;

    fn next(&mut self) -> Option<HilbertKey> {
        if self.cursor >= self.sidecar.count {
            return None;
        }
        if self.sidecar.read_id_at(self.cursor) != self.target {
            return None;
        }
        let key = self.sidecar.read_key_at(self.cursor);
        self.cursor += 1;
        Some(key)
    }
}

#[cfg(test)]
mod tests;

//! page-membership sidecar codec.
//!
//! per binding, a flat mmap-friendly binary file pairing every feature_id
//! with its hilbert key. used by the incremental compile path in C.2 to
//! resolve change-feed events to dirty page sets without round-tripping
//! through cells; written atomically alongside the manifest in the snapshot
//! path so the runtime / GFI can rely on the manifest's
//! `BindingMetadata::page_membership_sidecar` reference.
//!
//! on-disk format (little-endian):
//! ```text
//!   [magic "PMSC" u32][version u32 = 1][count u64]
//!   entries: count × [u64 feature_id][u64 hilbert_key]   // sorted by feature_id
//! ```
//! 16 bytes per feature -> ~800 MiB at 50M features (forvaltning2-class).

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
    #[error("sidecar: entries not sorted ascending by feature_id")]
    Unsorted,
    #[error("sidecar: duplicate feature_id {0}")]
    DuplicateFeatureId(u64),
    #[error("sidecar: count {0} exceeds u64 byte budget")]
    CountTooLarge(usize),
}

/// Encode a page-membership sidecar from `(feature_id, hilbert_key)` pairs.
/// the slice is sorted in place if not already; duplicate feature_ids are
/// rejected as a consistency violation (one feature -> one hilbert key per
/// snapshot).
pub fn encode_sidecar(entries: &mut [(u64, HilbertKey)]) -> Result<Bytes, SidecarError> {
    entries.sort_unstable_by_key(|(id, _)| *id);
    for w in entries.windows(2) {
        if w[0].0 == w[1].0 {
            return Err(SidecarError::DuplicateFeatureId(w[0].0));
        }
    }
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

        // verify ascending feature_ids and reject duplicates -- both invariants
        // are load-bearing for the binary-search lookup below.
        let mut prev: Option<u64> = None;
        for i in 0..count {
            let off = HEADER_LEN + i * ENTRY_LEN;
            let id = u64::from_le_bytes(bytes[off..off + 8].try_into().map_err(|_| SidecarError::Truncated)?);
            if let Some(p) = prev {
                if id == p {
                    return Err(SidecarError::DuplicateFeatureId(id));
                }
                if id < p {
                    return Err(SidecarError::Unsorted);
                }
            }
            prev = Some(id);
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

    /// Collect feature_ids whose hilbert key falls in any of `ranges`. Each
    /// range is `(lo, hi)` inclusive. Single-pass over the sidecar; used by
    /// the rebuild path to resolve dirty-page member sets.
    #[must_use]
    pub fn feature_ids_in_ranges(&self, ranges: &[(HilbertKey, HilbertKey)]) -> Vec<u64> {
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

    /// Binary-search for `feature_id`. `Ok(None)` when the id is absent.
    pub fn lookup(&self, feature_id: u64) -> Option<HilbertKey> {
        if self.count == 0 {
            return None;
        }
        let mut lo: usize = 0;
        let mut hi: usize = self.count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let off = HEADER_LEN + mid * ENTRY_LEN;
            // bounds were validated in `open`; entries[mid] sits in range.
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
            match id.cmp(&feature_id) {
                core::cmp::Ordering::Equal => {
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
                    return Some(HilbertKey::new(key));
                }
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
            }
        }
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn entries(n: u64) -> Vec<(u64, HilbertKey)> {
        (0..n)
            .map(|i| (i * 31 + 7, HilbertKey::new(i.wrapping_mul(0x9E37_79B9_7F4A_7C15))))
            .collect()
    }

    #[test]
    fn roundtrip_small() {
        let mut e = vec![
            (3u64, HilbertKey::new(30)),
            (1, HilbertKey::new(10)),
            (2, HilbertKey::new(20)),
        ];
        let bytes = encode_sidecar(&mut e).unwrap();
        let reader = SidecarReader::open(&bytes).unwrap();
        assert_eq!(reader.len(), 3);
        assert_eq!(reader.lookup(1), Some(HilbertKey::new(10)));
        assert_eq!(reader.lookup(2), Some(HilbertKey::new(20)));
        assert_eq!(reader.lookup(3), Some(HilbertKey::new(30)));
        assert_eq!(reader.lookup(0), None);
        assert_eq!(reader.lookup(99), None);
    }

    #[test]
    fn roundtrip_10k_random_lookups() {
        let mut e = entries(10_000);
        let oracle: std::collections::HashMap<u64, HilbertKey> = e.iter().copied().collect();
        let bytes = encode_sidecar(&mut e).unwrap();
        let reader = SidecarReader::open(&bytes).unwrap();
        assert_eq!(reader.len(), 10_000);
        // 100 spot checks via the oracle.
        let mut state = 1u64;
        for _ in 0..100 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let i = (state >> 32) % 10_000;
            let id = i * 31 + 7;
            assert_eq!(reader.lookup(id), oracle.get(&id).copied(), "mismatch at id {id}");
        }
        // an id known to be absent.
        assert_eq!(reader.lookup(8), None);
    }

    #[test]
    fn empty_sidecar_roundtrips() {
        let mut e: Vec<(u64, HilbertKey)> = vec![];
        let bytes = encode_sidecar(&mut e).unwrap();
        let reader = SidecarReader::open(&bytes).unwrap();
        assert!(reader.is_empty());
        assert_eq!(reader.lookup(0), None);
    }

    #[test]
    fn rejects_duplicate_feature_ids_at_encode() {
        let mut e = vec![(5u64, HilbertKey::new(1)), (5, HilbertKey::new(2))];
        let err = encode_sidecar(&mut e).unwrap_err();
        assert!(matches!(err, SidecarError::DuplicateFeatureId(5)));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut e = vec![(1u64, HilbertKey::new(2))];
        let bytes = encode_sidecar(&mut e).unwrap();
        let mut munged = bytes.to_vec();
        munged[0] ^= 0xff;
        assert!(matches!(SidecarReader::open(&munged), Err(SidecarError::BadHeader)));
    }

    #[test]
    fn rejects_truncated_buffer() {
        let mut e = vec![
            (1u64, HilbertKey::new(2)),
            (3u64, HilbertKey::new(4)),
            (5u64, HilbertKey::new(6)),
        ];
        let bytes = encode_sidecar(&mut e).unwrap();
        for cut in 0..bytes.len() {
            let truncated = &bytes[..cut];
            assert!(SidecarReader::open(truncated).is_err(), "should reject cut={cut}");
        }
    }

    #[test]
    fn rejects_bad_version() {
        let mut e = vec![(1u64, HilbertKey::new(2))];
        let bytes = encode_sidecar(&mut e).unwrap();
        let mut munged = bytes.to_vec();
        munged[4] = 99;
        assert!(matches!(SidecarReader::open(&munged), Err(SidecarError::BadHeader)));
    }

    #[test]
    fn iter_yields_all_pairs_in_feature_id_order() {
        let mut e = vec![
            (5u64, HilbertKey::new(50)),
            (1u64, HilbertKey::new(10)),
            (3u64, HilbertKey::new(30)),
        ];
        let bytes = encode_sidecar(&mut e).unwrap();
        let reader = SidecarReader::open(&bytes).unwrap();
        let pairs: Vec<(u64, HilbertKey)> = reader.iter().collect();
        assert_eq!(
            pairs,
            vec![
                (1, HilbertKey::new(10)),
                (3, HilbertKey::new(30)),
                (5, HilbertKey::new(50)),
            ]
        );
    }

    #[test]
    fn feature_ids_in_ranges_filters_by_key() {
        let mut e = vec![
            (10u64, HilbertKey::new(100)),
            (20u64, HilbertKey::new(200)),
            (30u64, HilbertKey::new(300)),
            (40u64, HilbertKey::new(400)),
            (50u64, HilbertKey::new(500)),
        ];
        let bytes = encode_sidecar(&mut e).unwrap();
        let reader = SidecarReader::open(&bytes).unwrap();
        let ranges = vec![
            (HilbertKey::new(150), HilbertKey::new(250)),
            (HilbertKey::new(350), HilbertKey::new(500)),
        ];
        let ids = reader.feature_ids_in_ranges(&ranges);
        assert_eq!(ids, vec![20, 40, 50]);

        // empty range list yields empty result.
        let empty = reader.feature_ids_in_ranges(&[]);
        assert!(empty.is_empty());
    }

    #[test]
    fn rejects_unsorted_handcrafted() {
        // build a sidecar by hand with descending feature_ids.
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC.to_le_bytes());
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&2u64.to_le_bytes());
        buf.extend_from_slice(&9u64.to_le_bytes());
        buf.extend_from_slice(&90u64.to_le_bytes());
        buf.extend_from_slice(&1u64.to_le_bytes());
        buf.extend_from_slice(&10u64.to_le_bytes());
        assert!(matches!(SidecarReader::open(&buf), Err(SidecarError::Unsorted)));
    }
}

//! tag-prefixed binary encoding for a row's attribute block.
//!
//! TODO: replace with apache arrow ipc per SPEC §9.3. this informal codec
//! is a phase-0 stub used by tests only; it lives here next to the geometry
//! codec because both encode artifact-side payloads. moving it later is
//! purely a port-vocabulary change.
//!
//! on-disk contract (little-endian, lengths as `u32`):
//!   block := count:u32, entry*
//!   entry := name_len:u32, name:utf8, tag:u8, payload
//!   payload by tag:
//!     0 Null    -> (none)
//!     1 Bool    -> u8 (0 | 1)
//!     2 Int     -> i64 LE
//!     3 Float   -> f64 LE (IEEE 754 bits)
//!     4 String  -> u32 len, utf8 bytes
//!
//! per-row block is bounded at 64 KiB to keep one bad row from exhausting
//! memory; oversize blocks return `AttrError::TooLarge`.

use bytes::Bytes;

/// on-disk attribute value vocabulary for the phase-0 codec. uses
/// [`mars_expr::Literal`] directly so the artifact codec, the expression
/// layer, and adapter conversions all speak the same shape — there used to
/// be a parallel `AttrValue` enum here that drifted (no `serde` derive) and
/// duplicated the same five variants.
pub use mars_expr::Literal as AttrValue;

/// Maximum encoded size of a single row's attribute block.
pub const MAX_ROW_BYTES: usize = 64 * 1024;

const TAG_NULL: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_INT: u8 = 2;
const TAG_FLOAT: u8 = 3;
const TAG_STRING: u8 = 4;

/// Magic prefix for the attributes section payload (LE bytes "MARSATTR").
const SECTION_MAGIC: &[u8; 8] = b"MARSATTR";

/// Per-entry size of the directory at the tail of the attributes section
/// (`[u64 feature_id][u32 byte_offset]`).
const DIR_ENTRY_LEN: usize = 12;

/// Section header is `[magic 8][version u32][count u32][dir_offset u32]`.
const SECTION_HEADER_LEN: usize = 8 + 4 + 4 + 4;

const SECTION_VERSION: u32 = 1;

/// Errors raised while decoding or encoding an attribute block.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AttrError {
    /// Block exceeds `MAX_ROW_BYTES`.
    #[error("row block too large: {got} > {max}")]
    TooLarge {
        /// Observed block size.
        got: usize,
        /// Configured cap.
        max: usize,
    },
    /// A field exceeds the encoder's representable size (e.g. >4 GiB string,
    /// or row count beyond `u32::MAX`). `kind` names the offending field.
    #[error("input too large to encode: {kind}")]
    InputTooLarge {
        /// Human-readable label of the field (e.g. "string", "row count").
        kind: &'static str,
    },
    /// Buffer ended mid-record.
    #[error("unexpected end of input")]
    UnexpectedEof,
    /// Tag byte is not in {0..=4}.
    #[error("unknown tag: {0}")]
    UnknownTag(u8),
    /// String / name field was not valid UTF-8.
    #[error("invalid utf-8")]
    InvalidUtf8,
    /// Length prefix beyond `u32::MAX` or beyond the remaining buffer.
    #[error("length out of range")]
    BadLength,
    /// Trailing data after the declared entry count.
    #[error("trailing bytes after row block")]
    TrailingBytes,
    /// Section magic / version did not match the expected attributes section.
    #[error("attributes section: bad magic or version")]
    SectionBadHeader,
    /// Section directory is malformed (offset out of range, length not a
    /// multiple of `DIR_ENTRY_LEN`, declared count would overflow).
    #[error("attributes section: bad directory")]
    SectionBadDirectory,
    /// Section directory is not sorted ascending by feature_id.
    #[error("attributes section: directory not sorted")]
    SectionUnsorted,
    /// Two entries in the section share the same feature_id.
    #[error("attributes section: duplicate feature_id {0}")]
    SectionDuplicateFeatureId(u64),
}

/// Encode an ordered slice of `(name, AttrValue)` pairs to bytes.
///
/// Returns `AttrError::InputTooLarge` if the row count exceeds `u32::MAX` or
/// any string field exceeds `u32::MAX` bytes.
///
/// **Phase-0 codec, scheduled for replacement.** SPEC §9.3 mandates Apache
/// Arrow IPC for the attributes section; this informal tag-prefixed format is
/// a stub used by tests and a few internal call sites until the Arrow port
/// lands. Do not accumulate new callers around it — when the swap happens,
/// this signature will be removed in favour of an Arrow-shaped one.
pub fn encode_row(values: &[(String, AttrValue)]) -> Result<Bytes, AttrError> {
    let count = u32::try_from(values.len()).map_err(|_| AttrError::InputTooLarge { kind: "row count" })?;
    let cap = estimate_size(values);
    let mut out = Vec::with_capacity(cap);
    out.extend_from_slice(&count.to_le_bytes());
    for (name, v) in values {
        write_string(&mut out, name)?;
        match v {
            AttrValue::Null => out.push(TAG_NULL),
            AttrValue::Bool(b) => {
                out.push(TAG_BOOL);
                out.push(u8::from(*b));
            }
            AttrValue::Int(i) => {
                out.push(TAG_INT);
                out.extend_from_slice(&i.to_le_bytes());
            }
            AttrValue::Float(f) => {
                out.push(TAG_FLOAT);
                out.extend_from_slice(&f.to_le_bytes());
            }
            AttrValue::String(s) => {
                out.push(TAG_STRING);
                write_string(&mut out, s)?;
            }
        }
    }
    Ok(Bytes::from(out))
}

/// Decode a `(name, AttrValue)` block. Rejects blocks larger than
/// `MAX_ROW_BYTES` before parsing.
///
/// **Phase-0 codec, scheduled for replacement** (see [`encode_row`]).
pub fn decode_row(bytes: &[u8]) -> Result<Vec<(String, AttrValue)>, AttrError> {
    if bytes.len() > MAX_ROW_BYTES {
        return Err(AttrError::TooLarge {
            got: bytes.len(),
            max: MAX_ROW_BYTES,
        });
    }
    let mut c = Cursor::new(bytes);
    let count = c.read_u32()? as usize;
    // refuse to allocate beyond the buffer-bound estimate; an entry is
    // minimally a 4-byte name length, a 1-byte tag, and a 0-byte name.
    const MIN_ENTRY_LEN: usize = 4 + 1;
    let max_possible = bytes.len().saturating_sub(4) / MIN_ENTRY_LEN;
    if count > max_possible {
        return Err(AttrError::UnexpectedEof);
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let name = c.read_string()?;
        let tag = c.read_u8()?;
        let v = match tag {
            TAG_NULL => AttrValue::Null,
            TAG_BOOL => AttrValue::Bool(c.read_u8()? != 0),
            TAG_INT => AttrValue::Int(c.read_i64()?),
            TAG_FLOAT => AttrValue::Float(c.read_f64()?),
            TAG_STRING => AttrValue::String(c.read_string()?),
            other => return Err(AttrError::UnknownTag(other)),
        };
        out.push((name, v));
    }
    if !c.is_empty() {
        return Err(AttrError::TrailingBytes);
    }
    Ok(out)
}

fn estimate_size(values: &[(String, AttrValue)]) -> usize {
    let mut n = 4;
    for (name, v) in values {
        n += 4 + name.len() + 1;
        n += match v {
            AttrValue::Null => 0,
            AttrValue::Bool(_) => 1,
            AttrValue::Int(_) | AttrValue::Float(_) => 8,
            AttrValue::String(s) => 4 + s.len(),
        };
    }
    n
}

fn write_string(out: &mut Vec<u8>, s: &str) -> Result<(), AttrError> {
    let len = u32::try_from(s.len()).map_err(|_| AttrError::InputTooLarge { kind: "string" })?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], AttrError> {
        let end = self.pos.checked_add(n).ok_or(AttrError::BadLength)?;
        if end > self.buf.len() {
            return Err(AttrError::UnexpectedEof);
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn read_u8(&mut self) -> Result<u8, AttrError> {
        Ok(self.take(1)?[0])
    }

    fn read_u32(&mut self) -> Result<u32, AttrError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_i64(&mut self) -> Result<i64, AttrError> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(i64::from_le_bytes(a))
    }

    fn read_f64(&mut self) -> Result<f64, AttrError> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(f64::from_le_bytes(a))
    }

    fn read_string(&mut self) -> Result<String, AttrError> {
        let len = self.read_u32()? as usize;
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| AttrError::InvalidUtf8)
    }
}

/// Encode an attributes section: a directory-indexed bundle of per-feature
/// rows. Each `row_bytes` is whatever the per-row codec produced (typically
/// [`encode_row`]). The section pairs every row with its `feature_id` so the
/// reader can binary-search by id without scanning rows.
///
/// Layout (little-endian):
/// ```text
///   [magic "MARSATTR"][version u32 = 1][count u32][dir_offset u32]
///   rows region: count × [u32 row_len][row_bytes]
///   directory region (at dir_offset, sorted by feature_id):
///     count × [u64 feature_id][u32 byte_offset]
/// ```
/// `byte_offset` points at the row's `[u32 row_len]` length prefix, measured
/// from the start of the section payload.
pub fn encode_attributes_section(rows: &[(u64, &[u8])]) -> Result<Bytes, AttrError> {
    let count = u32::try_from(rows.len()).map_err(|_| AttrError::InputTooLarge { kind: "row count" })?;

    // first pass: stable sort a (feature_id, original_index) helper so callers
    // need not pre-sort. duplicates are rejected up front.
    let mut order: Vec<(u64, usize)> = rows.iter().enumerate().map(|(i, (id, _))| (*id, i)).collect();
    order.sort_unstable_by_key(|(id, _)| *id);
    for w in order.windows(2) {
        if w[0].0 == w[1].0 {
            return Err(AttrError::SectionDuplicateFeatureId(w[0].0));
        }
    }

    // second pass: emit rows in feature_id-sorted order, recording each row's
    // byte offset (relative to section payload start) for the directory.
    let mut out = Vec::with_capacity(SECTION_HEADER_LEN + rows.len() * 16);
    out.extend_from_slice(SECTION_MAGIC);
    out.extend_from_slice(&SECTION_VERSION.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    // dir_offset is patched in once rows are written.
    out.extend_from_slice(&0u32.to_le_bytes());

    let mut offsets: Vec<u32> = Vec::with_capacity(rows.len());
    for (_id, original_idx) in &order {
        let payload = rows[*original_idx].1;
        let row_len = u32::try_from(payload.len()).map_err(|_| AttrError::InputTooLarge { kind: "row payload" })?;
        let off = u32::try_from(out.len()).map_err(|_| AttrError::InputTooLarge { kind: "row offset" })?;
        offsets.push(off);
        out.extend_from_slice(&row_len.to_le_bytes());
        out.extend_from_slice(payload);
    }

    let dir_offset = u32::try_from(out.len()).map_err(|_| AttrError::InputTooLarge {
        kind: "directory offset",
    })?;
    // directory: count × [u64 feature_id][u32 byte_offset], sorted ascending.
    for (i, (id, _)) in order.iter().enumerate() {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&offsets[i].to_le_bytes());
    }

    // patch dir_offset in the header (bytes 16..20).
    out[16..20].copy_from_slice(&dir_offset.to_le_bytes());
    Ok(Bytes::from(out))
}

/// Read-only view over an attributes section payload. Borrows the underlying
/// bytes so lookup is zero-copy.
#[derive(Debug, Clone, Copy)]
pub struct AttributesSection<'a> {
    bytes: &'a [u8],
    count: u32,
    dir_offset: usize,
}

impl<'a> AttributesSection<'a> {
    /// Validate the section header + directory and capture the dimensions.
    /// Cheap: O(1). Per-row payload bounds are re-checked at lookup time.
    pub fn open(bytes: &'a [u8]) -> Result<Self, AttrError> {
        if bytes.len() < SECTION_HEADER_LEN {
            return Err(AttrError::SectionBadHeader);
        }
        if &bytes[..8] != SECTION_MAGIC {
            return Err(AttrError::SectionBadHeader);
        }
        let version = u32::from_le_bytes(bytes[8..12].try_into().map_err(|_| AttrError::SectionBadHeader)?);
        if version != SECTION_VERSION {
            return Err(AttrError::SectionBadHeader);
        }
        let count = u32::from_le_bytes(bytes[12..16].try_into().map_err(|_| AttrError::SectionBadHeader)?);
        let dir_offset =
            u32::from_le_bytes(bytes[16..20].try_into().map_err(|_| AttrError::SectionBadHeader)?) as usize;

        // directory must lie wholly inside the buffer and have the right length.
        let dir_len = (count as usize)
            .checked_mul(DIR_ENTRY_LEN)
            .ok_or(AttrError::SectionBadDirectory)?;
        let dir_end = dir_offset.checked_add(dir_len).ok_or(AttrError::SectionBadDirectory)?;
        if dir_offset < SECTION_HEADER_LEN || dir_end != bytes.len() {
            return Err(AttrError::SectionBadDirectory);
        }

        // verify ascending order so binary search is meaningful, and guard
        // against forged blobs.
        let mut prev: Option<u64> = None;
        for i in 0..(count as usize) {
            let off = dir_offset + i * DIR_ENTRY_LEN;
            let id = u64::from_le_bytes(
                bytes[off..off + 8]
                    .try_into()
                    .map_err(|_| AttrError::SectionBadDirectory)?,
            );
            if let Some(p) = prev {
                if id == p {
                    return Err(AttrError::SectionDuplicateFeatureId(id));
                }
                if id < p {
                    return Err(AttrError::SectionUnsorted);
                }
            }
            prev = Some(id);
        }

        Ok(Self {
            bytes,
            count,
            dir_offset,
        })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.count as usize
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Binary-search the directory and return the row payload slice for
    /// `feature_id`, or `None` when the id is absent.
    pub fn lookup(&self, feature_id: u64) -> Result<Option<&'a [u8]>, AttrError> {
        if self.count == 0 {
            return Ok(None);
        }
        let mut lo: usize = 0;
        let mut hi: usize = self.count as usize;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let off = self.dir_offset + mid * DIR_ENTRY_LEN;
            let id = u64::from_le_bytes(
                self.bytes[off..off + 8]
                    .try_into()
                    .map_err(|_| AttrError::SectionBadDirectory)?,
            );
            match id.cmp(&feature_id) {
                core::cmp::Ordering::Equal => {
                    let row_off = u32::from_le_bytes(
                        self.bytes[off + 8..off + 12]
                            .try_into()
                            .map_err(|_| AttrError::SectionBadDirectory)?,
                    ) as usize;
                    if row_off.checked_add(4).ok_or(AttrError::SectionBadDirectory)? > self.bytes.len() {
                        return Err(AttrError::SectionBadDirectory);
                    }
                    let row_len = u32::from_le_bytes(
                        self.bytes[row_off..row_off + 4]
                            .try_into()
                            .map_err(|_| AttrError::SectionBadDirectory)?,
                    ) as usize;
                    let row_end = row_off
                        .checked_add(4)
                        .and_then(|v| v.checked_add(row_len))
                        .ok_or(AttrError::SectionBadDirectory)?;
                    if row_end > self.dir_offset {
                        // a row would overrun into the directory
                        return Err(AttrError::SectionBadDirectory);
                    }
                    return Ok(Some(&self.bytes[row_off + 4..row_end]));
                }
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn roundtrip_all_variants() {
        let row = vec![
            ("n".into(), AttrValue::Null),
            ("b".into(), AttrValue::Bool(true)),
            ("i".into(), AttrValue::Int(-42)),
            ("f".into(), AttrValue::Float(2.5)),
            ("s".into(), AttrValue::String("hello".into())),
        ];
        let bytes = encode_row(&row).unwrap();
        let back = decode_row(&bytes).unwrap();
        assert_eq!(back, row);
    }

    #[test]
    fn empty_row_roundtrips() {
        let bytes = encode_row(&[]).unwrap();
        assert_eq!(decode_row(&bytes).unwrap(), Vec::new());
    }

    #[test]
    fn huge_row_count_in_header_rejected() {
        // declare u32::MAX entries in a tiny buffer; must not allocate
        let mut buf = Vec::new();
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(decode_row(&buf), Err(AttrError::UnexpectedEof)));
    }

    #[test]
    fn oversize_block_rejected() {
        let big = vec![0u8; MAX_ROW_BYTES + 1];
        assert!(matches!(decode_row(&big), Err(AttrError::TooLarge { .. })));
    }

    #[test]
    fn unknown_tag_rejected() {
        // 1 entry, name "x", tag=99
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.push(b'x');
        buf.push(99);
        assert!(matches!(decode_row(&buf), Err(AttrError::UnknownTag(99))));
    }

    #[test]
    fn truncated_input_rejected() {
        let row = vec![("k".into(), AttrValue::Int(1))];
        let bytes = encode_row(&row).unwrap();
        let truncated = &bytes[..bytes.len() - 1];
        assert!(matches!(decode_row(truncated), Err(AttrError::UnexpectedEof)));
    }

    fn arb_attr() -> impl Strategy<Value = AttrValue> {
        prop_oneof![
            Just(AttrValue::Null),
            any::<bool>().prop_map(AttrValue::Bool),
            any::<i64>().prop_map(AttrValue::Int),
            any::<f64>()
                .prop_filter("finite", |f| f.is_finite())
                .prop_map(AttrValue::Float),
            ".{0,32}".prop_map(AttrValue::String),
        ]
    }

    proptest! {
        #[test]
        fn roundtrip_random(rows in proptest::collection::vec(("[a-z]{1,8}".prop_map(String::from), arb_attr()), 0..16)) {
            let bytes = encode_row(&rows).unwrap();
            prop_assume!(bytes.len() <= MAX_ROW_BYTES);
            let back = decode_row(&bytes).unwrap();
            prop_assert_eq!(back, rows);
        }
    }

    fn encoded(values: &[(&str, AttrValue)]) -> Vec<u8> {
        let owned: Vec<(String, AttrValue)> = values.iter().map(|(k, v)| ((*k).into(), v.clone())).collect();
        encode_row(&owned).unwrap().to_vec()
    }

    #[test]
    fn section_roundtrip_small() {
        let r1 = encoded(&[("name", AttrValue::String("a".into())), ("k", AttrValue::Int(1))]);
        let r2 = encoded(&[("name", AttrValue::String("b".into())), ("k", AttrValue::Int(2))]);
        let r3 = encoded(&[("name", AttrValue::String("c".into())), ("k", AttrValue::Int(3))]);
        let bytes = encode_attributes_section(&[(7, &r1), (3, &r2), (42, &r3)]).unwrap();
        let sec = AttributesSection::open(&bytes).unwrap();
        assert_eq!(sec.len(), 3);

        let got = sec.lookup(3).unwrap().unwrap();
        assert_eq!(decode_row(got).unwrap(), decode_row(&r2).unwrap());
        let got = sec.lookup(7).unwrap().unwrap();
        assert_eq!(decode_row(got).unwrap(), decode_row(&r1).unwrap());
        let got = sec.lookup(42).unwrap().unwrap();
        assert_eq!(decode_row(got).unwrap(), decode_row(&r3).unwrap());

        assert!(sec.lookup(0).unwrap().is_none());
        assert!(sec.lookup(99).unwrap().is_none());
    }

    #[test]
    fn section_roundtrip_1k() {
        let mut rows: Vec<(u64, Vec<u8>)> = (0..1000)
            .map(|i| {
                let payload = encoded(&[("k", AttrValue::Int(i as i64))]);
                (i as u64 * 31 + 5, payload)
            })
            .collect();
        let refs: Vec<(u64, &[u8])> = rows.iter().map(|(id, p)| (*id, p.as_slice())).collect();
        let bytes = encode_attributes_section(&refs).unwrap();
        let sec = AttributesSection::open(&bytes).unwrap();

        // sample 100 ids and confirm decoded rows match.
        for i in (0..1000).step_by(10) {
            let id = i as u64 * 31 + 5;
            let got = sec.lookup(id).unwrap().unwrap();
            let expected = &rows[i].1;
            assert_eq!(got, expected.as_slice(), "row {id}");
        }
        // a missing id between two present ones falls through.
        assert!(sec.lookup(rows[0].0 + 1).unwrap().is_none());
        rows.clear();
    }

    #[test]
    fn section_empty() {
        let bytes = encode_attributes_section(&[]).unwrap();
        let sec = AttributesSection::open(&bytes).unwrap();
        assert!(sec.is_empty());
        assert!(sec.lookup(0).unwrap().is_none());
        assert!(sec.lookup(u64::MAX).unwrap().is_none());
    }

    #[test]
    fn section_rejects_duplicate_feature_ids_at_encode() {
        let r = encoded(&[("k", AttrValue::Int(1))]);
        let err = encode_attributes_section(&[(5, &r), (5, &r)]).unwrap_err();
        assert!(matches!(err, AttrError::SectionDuplicateFeatureId(5)));
    }

    #[test]
    fn section_rejects_truncated_buffer() {
        let r = encoded(&[("k", AttrValue::Int(1))]);
        let bytes = encode_attributes_section(&[(1, &r)]).unwrap();
        for cut in 0..bytes.len() {
            let truncated = &bytes[..cut];
            assert!(AttributesSection::open(truncated).is_err(), "should reject cut={cut}");
        }
    }

    #[test]
    fn section_rejects_bad_magic() {
        let r = encoded(&[("k", AttrValue::Int(1))]);
        let bytes = encode_attributes_section(&[(1, &r)]).unwrap();
        let mut munged = bytes.to_vec();
        munged[0] ^= 0xff;
        assert!(matches!(
            AttributesSection::open(&munged),
            Err(AttrError::SectionBadHeader)
        ));
    }

    #[test]
    fn section_rejects_unsorted_directory() {
        // hand-craft a section with a directory that is sorted descending.
        let r = encoded(&[("k", AttrValue::Int(1))]);
        let mut buf = Vec::new();
        buf.extend_from_slice(SECTION_MAGIC);
        buf.extend_from_slice(&SECTION_VERSION.to_le_bytes());
        buf.extend_from_slice(&2u32.to_le_bytes()); // count
        buf.extend_from_slice(&0u32.to_le_bytes()); // dir_offset placeholder

        let off1 = buf.len() as u32;
        buf.extend_from_slice(&(r.len() as u32).to_le_bytes());
        buf.extend_from_slice(&r);
        let off2 = buf.len() as u32;
        buf.extend_from_slice(&(r.len() as u32).to_le_bytes());
        buf.extend_from_slice(&r);

        let dir_off = buf.len() as u32;
        // descending: 9 then 1.
        buf.extend_from_slice(&9u64.to_le_bytes());
        buf.extend_from_slice(&off1.to_le_bytes());
        buf.extend_from_slice(&1u64.to_le_bytes());
        buf.extend_from_slice(&off2.to_le_bytes());

        // patch dir_offset.
        buf[16..20].copy_from_slice(&dir_off.to_le_bytes());

        assert!(matches!(AttributesSection::open(&buf), Err(AttrError::SectionUnsorted)));
    }
}

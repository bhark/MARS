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
}

//! LEB128 varint + zigzag helpers for signed deltas.

use crate::ArtifactError;

pub(crate) fn write_uvarint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub(crate) fn read_uvarint(buf: &[u8], pos: &mut usize) -> Result<u64, ArtifactError> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        if *pos >= buf.len() {
            return Err(ArtifactError::Truncated);
        }
        let b = buf[*pos];
        *pos += 1;
        let payload = u64::from(b & 0x7F);

        if shift == 63 {
            // 10th and final byte: only bit 63 remains; payload must be 0 or 1,
            // and there must be no continuation bit.
            if b & 0x80 != 0 {
                return Err(ArtifactError::Malformed("varint overflow"));
            }
            if payload > 1 {
                return Err(ArtifactError::Malformed("varint overflow"));
            }
            result |= payload << shift;
            return Ok(result);
        }

        result |= payload << shift;
        if b & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift > 63 {
            return Err(ArtifactError::Malformed("varint overflow"));
        }
    }
}

#[inline]
pub(crate) fn zigzag_encode(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
pub(crate) fn zigzag_decode(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

pub(crate) fn write_ivarint(out: &mut Vec<u8>, v: i64) {
    write_uvarint(out, zigzag_encode(v));
}

pub(crate) fn read_ivarint(buf: &[u8], pos: &mut usize) -> Result<i64, ArtifactError> {
    Ok(zigzag_decode(read_uvarint(buf, pos)?))
}

//! style_refs section codec: u32 count, then (u32 length, utf8 bytes) entries.

use bytes::Bytes;

use crate::ArtifactError;

#[must_use]
pub fn encode_style_refs(refs: &[String]) -> Bytes {
    let total_str: usize = refs.iter().map(|s| s.len()).sum();
    let mut out = Vec::with_capacity(4 + refs.len() * 4 + total_str);
    out.extend_from_slice(&(refs.len() as u32).to_le_bytes());
    for s in refs {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    Bytes::from(out)
}

// smallest possible entry: u32 length prefix with zero-byte string body
const MIN_ENTRY_LEN: usize = 4;

pub fn decode_style_refs(bytes: &[u8]) -> Result<Vec<String>, ArtifactError> {
    if bytes.len() < 4 {
        return Err(ArtifactError::Truncated);
    }
    let n = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    // cap allocation: even an empty entry is MIN_ENTRY_LEN bytes; refuse to
    // pre-size beyond what the buffer could possibly hold.
    let max_possible = bytes.len().saturating_sub(4) / MIN_ENTRY_LEN;
    if n > max_possible {
        return Err(ArtifactError::Truncated);
    }
    let mut pos = 4;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        if bytes.len() < pos + 4 {
            return Err(ArtifactError::Truncated);
        }
        let len = u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]) as usize;
        pos += 4;
        if bytes.len() < pos + len {
            return Err(ArtifactError::Truncated);
        }
        let s = std::str::from_utf8(&bytes[pos..pos + len])
            .map_err(|_| ArtifactError::Malformed("style_ref utf8"))?
            .to_owned();
        pos += len;
        out.push(s);
    }
    if pos != bytes.len() {
        return Err(ArtifactError::Malformed("style_refs trailing bytes"));
    }
    Ok(out)
}

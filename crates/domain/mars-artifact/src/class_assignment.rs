//! class_assignment section codec: ascending [(u32 feature_idx, u16 class_index)].
//!
//! Sparse: entries cover only the slots that have a class match. Decoders
//! typically materialise into a `Vec<Option<u16>>` indexed by slot for O(1)
//! lookup.

use bytes::Bytes;

use crate::ArtifactError;

const ENTRY_LEN: usize = 4 + 2;

/// encoder mirrors decoder's invariants: feature_idx strictly ascending, no
/// duplicates (at most one class per slot). validating at encode time prevents
/// producing artifacts that would only fail at runtime decode.
pub fn encode_class_assignment(items: &[(u32, u16)]) -> Result<Bytes, ArtifactError> {
    let mut prev: Option<u32> = None;
    for (idx, _) in items {
        if let Some(p) = prev {
            if *idx == p {
                return Err(ArtifactError::Malformed("duplicate feature_idx"));
            }
            if *idx < p {
                return Err(ArtifactError::Malformed(
                    "class assignments must be ascending by feature_idx",
                ));
            }
        }
        prev = Some(*idx);
    }
    let mut out = Vec::with_capacity(4 + items.len() * ENTRY_LEN);
    out.extend_from_slice(&(items.len() as u32).to_le_bytes());
    for (idx, cls) in items {
        out.extend_from_slice(&idx.to_le_bytes());
        out.extend_from_slice(&cls.to_le_bytes());
    }
    Ok(Bytes::from(out))
}

pub fn decode_class_assignment(bytes: &[u8]) -> Result<Vec<(u32, u16)>, ArtifactError> {
    if bytes.len() < 4 {
        return Err(ArtifactError::Truncated);
    }
    let n = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let expected = 4usize
        .checked_add(n.checked_mul(ENTRY_LEN).ok_or(ArtifactError::Truncated)?)
        .ok_or(ArtifactError::Truncated)?;
    if bytes.len() < expected {
        return Err(ArtifactError::Truncated);
    }
    if bytes.len() > expected {
        return Err(ArtifactError::Malformed("trailing bytes"));
    }
    let mut out = Vec::with_capacity(n);
    let mut prev: Option<u32> = None;
    for i in 0..n {
        let off = 4 + i * ENTRY_LEN;
        let idx = u32::from_le_bytes(bytes[off..off + 4].try_into().map_err(|_| ArtifactError::Truncated)?);
        let cls = u16::from_le_bytes([bytes[off + 4], bytes[off + 5]]);
        if let Some(p) = prev {
            if idx == p {
                return Err(ArtifactError::Malformed("duplicate feature_idx"));
            }
            if idx < p {
                return Err(ArtifactError::Malformed(
                    "class assignments must be ascending by feature_idx",
                ));
            }
        }
        prev = Some(idx);
        out.push((idx, cls));
    }
    Ok(out)
}

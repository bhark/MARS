//! class_assignment section codec: ascending [(u64 feature_id, u16 class_index)].

use bytes::Bytes;

use crate::ArtifactError;

const ENTRY_LEN: usize = 8 + 2;

#[must_use]
pub fn encode_class_assignment(items: &[(u64, u16)]) -> Bytes {
    let mut out = Vec::with_capacity(4 + items.len() * ENTRY_LEN);
    out.extend_from_slice(&(items.len() as u32).to_le_bytes());
    for (id, cls) in items {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&cls.to_le_bytes());
    }
    Bytes::from(out)
}

pub fn decode_class_assignment(bytes: &[u8]) -> Result<Vec<(u64, u16)>, ArtifactError> {
    if bytes.len() < 4 {
        return Err(ArtifactError::Truncated);
    }
    let n = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if bytes.len() != 4 + n * ENTRY_LEN {
        return Err(ArtifactError::Truncated);
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = 4 + i * ENTRY_LEN;
        let id = u64::from_le_bytes(
            bytes[off..off + 8]
                .try_into()
                .map_err(|_| ArtifactError::Truncated)?,
        );
        let cls = u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]);
        out.push((id, cls));
    }
    Ok(out)
}

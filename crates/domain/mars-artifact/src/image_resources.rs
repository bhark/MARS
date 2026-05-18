//! image_resources section codec.
//!
//! bundles raster bitmap assets referenced by `FillPaint::Image { name }` into
//! the published artifact so a runtime renderer can resolve `name -> bytes`
//! without out-of-band coordination with the source style author.
//!
//! Wire format (little-endian throughout):
//!
//! ```text
//! u32 count
//! repeat count times:
//!   u16 name_len
//!   name_len bytes utf-8 (non-empty, unique, ascending)
//!   u32 image_byte_len
//!   image_byte_len bytes (encoded image, format inferred at decode time)
//! ```
//!
//! Names are byte-sorted ascending and unique so a reader can `binary_search`
//! by name in `O(log n)` against a pre-built directory.

use bytes::Bytes;

use crate::ArtifactError;

/// One image resource. `bytes` is the encoded image (PNG / JPEG / WebP);
/// the decoder downstream sniffs the format from the bytes themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageResource {
    /// Stable name, referenced from styles as `FillPaint::Image { name }`.
    pub name: String,
    /// Encoded image bytes.
    pub bytes: Bytes,
}

// max bytes per name; capped at u16::MAX to match the on-wire length prefix.
pub(crate) const MAX_NAME_BYTES: usize = u16::MAX as usize;
// max bytes per image payload; capped at u32::MAX to match the on-wire length
// prefix. practical limits live in the compiler's image_pack stage.
pub(crate) const MAX_IMAGE_BYTES: usize = u32::MAX as usize;

// minimum bytes any entry occupies: u16 name_len + u32 image_byte_len = 6.
// the name itself must be non-empty (>=1 byte) and the image bytes can be 0
// (a zero-byte image is a malformed asset; encoder rejects it).
const MIN_ENTRY_LEN: usize = 2 + 4;

/// Encode a slice of [`ImageResource`] entries. Validates:
/// - names are non-empty and unique
/// - names are ascending (stable binary-search invariant)
/// - name bytes fit `u16`, image bytes fit `u32`
/// - image payload is non-empty
pub fn encode_image_resources(items: &[ImageResource]) -> Result<Bytes, ArtifactError> {
    let mut prev: Option<&str> = None;
    for it in items {
        if it.name.is_empty() {
            return Err(ArtifactError::Malformed("image resource name is empty"));
        }
        if it.name.len() > MAX_NAME_BYTES {
            return Err(ArtifactError::Malformed("image resource name exceeds max bytes"));
        }
        if it.bytes.is_empty() {
            return Err(ArtifactError::Malformed("image resource payload is empty"));
        }
        if it.bytes.len() > MAX_IMAGE_BYTES {
            return Err(ArtifactError::Malformed("image resource payload exceeds max bytes"));
        }
        if let Some(p) = prev
            && it.name.as_str() <= p
        {
            return Err(ArtifactError::Malformed(
                "image resources must be ascending and unique by name",
            ));
        }
        prev = Some(it.name.as_str());
    }
    let count = u32::try_from(items.len()).map_err(|_| ArtifactError::Malformed("image resource count exceeds u32"))?;
    let mut size = 4usize;
    for it in items {
        size = size
            .checked_add(MIN_ENTRY_LEN + it.name.len() + it.bytes.len())
            .ok_or(ArtifactError::Malformed("image resources section size overflow"))?;
    }
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&count.to_le_bytes());
    for it in items {
        let nlen = it.name.len() as u16;
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(it.name.as_bytes());
        let ilen = it.bytes.len() as u32;
        out.extend_from_slice(&ilen.to_le_bytes());
        out.extend_from_slice(&it.bytes);
    }
    Ok(Bytes::from(out))
}

/// Decode the inverse of [`encode_image_resources`]. Returns entries in the
/// stored ascending order; the caller can binary-search by name.
pub fn decode_image_resources(bytes: &[u8]) -> Result<Vec<ImageResource>, ArtifactError> {
    if bytes.len() < 4 {
        return Err(ArtifactError::Truncated);
    }
    let n = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let max_possible = bytes.len().saturating_sub(4) / MIN_ENTRY_LEN;
    if n > max_possible {
        return Err(ArtifactError::Truncated);
    }
    let mut pos = 4;
    let mut out = Vec::with_capacity(n);
    let mut prev: Option<String> = None;
    for _ in 0..n {
        if bytes.len() < pos + 2 {
            return Err(ArtifactError::Truncated);
        }
        let nlen = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        pos += 2;
        if nlen == 0 {
            return Err(ArtifactError::Malformed("image resource name is empty"));
        }
        if bytes.len() < pos + nlen {
            return Err(ArtifactError::Truncated);
        }
        let name = std::str::from_utf8(&bytes[pos..pos + nlen])
            .map_err(|_| ArtifactError::Malformed("image resource name utf8"))?
            .to_owned();
        pos += nlen;
        if bytes.len() < pos + 4 {
            return Err(ArtifactError::Truncated);
        }
        let ilen = u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]) as usize;
        pos += 4;
        if ilen == 0 {
            return Err(ArtifactError::Malformed("image resource payload is empty"));
        }
        if bytes.len() < pos + ilen {
            return Err(ArtifactError::Truncated);
        }
        let payload = Bytes::copy_from_slice(&bytes[pos..pos + ilen]);
        pos += ilen;
        if let Some(ref p) = prev
            && &name <= p
        {
            return Err(ArtifactError::Malformed(
                "image resources must be ascending and unique by name",
            ));
        }
        prev = Some(name.clone());
        out.push(ImageResource { name, bytes: payload });
    }
    if pos != bytes.len() {
        return Err(ArtifactError::Malformed("image_resources trailing bytes"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests;

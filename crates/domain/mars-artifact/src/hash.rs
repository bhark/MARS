//! BLAKE3 content hash over a complete artifact byte buffer.

use mars_types::ContentHash;

#[must_use]
pub fn compute_content_hash(bytes: &[u8]) -> ContentHash {
    let h = blake3::hash(bytes);
    ContentHash(*h.as_bytes())
}

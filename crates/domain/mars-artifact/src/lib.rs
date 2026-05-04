//! mars artifact container codec. on-disk layout per SPEC §9.3.
//! synchronous reader over `&[u8]` / `bytes::Bytes`; async i/o stays in adapters.

#![forbid(unsafe_code)]

use bytes::Bytes;

/// MARS magic bytes — also used as the trailer.
pub const MAGIC: &[u8; 8] = b"MARS\0\0\0\0";

/// Format version of the on-disk container. Bumped on incompatible changes.
pub const FORMAT_VERSION: u32 = 1;

include!(concat!(env!("OUT_DIR"), "/generated.rs"));

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("not a MARS artifact (bad magic)")]
    BadMagic,
    #[error("unsupported format version {0}")]
    UnsupportedVersion(u32),
    #[error("truncated artifact")]
    Truncated,
    #[error("not implemented: {what}")]
    NotImplemented { what: &'static str },
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SectionKind {
    GeometryIndex = 0x01,
    GeometryPayload = 0x02,
    Attributes = 0x03,
    LabelCandidates = 0x04,
    ClassAssignment = 0x05,
    StyleRefs = 0x06,
}

/// Synchronous mmap reader over an artifact's bytes.
#[derive(Debug)]
pub struct ArtifactReader {
    // held for phase 1 section access; only read by `open` today
    #[allow(dead_code)]
    bytes: Bytes,
}

impl ArtifactReader {
    /// Parses the artifact header and returns a reader. Phase 0: validates
    /// magic + version only; section access lands in Phase 1.
    pub fn open(bytes: Bytes) -> Result<Self, ArtifactError> {
        if bytes.len() < MAGIC.len() + 4 + 4 {
            return Err(ArtifactError::Truncated);
        }
        if &bytes[..MAGIC.len()] != MAGIC {
            return Err(ArtifactError::BadMagic);
        }
        let version = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        if version != FORMAT_VERSION {
            return Err(ArtifactError::UnsupportedVersion(version));
        }
        Ok(Self { bytes })
    }

    pub fn section(&self, _kind: SectionKind) -> Result<&[u8], ArtifactError> {
        Err(ArtifactError::NotImplemented {
            what: "ArtifactReader::section",
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn open_rejects_truncated() {
        let r = ArtifactReader::open(Bytes::from_static(b"abc"));
        assert!(matches!(r, Err(ArtifactError::Truncated)));
    }

    #[test]
    fn open_rejects_bad_magic() {
        let mut buf = vec![0u8; 16];
        buf[..8].copy_from_slice(b"NOTMARS!");
        let r = ArtifactReader::open(Bytes::from(buf));
        assert!(matches!(r, Err(ArtifactError::BadMagic)));
    }

    #[test]
    fn open_accepts_valid_header() {
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // flags
        let r = ArtifactReader::open(Bytes::from(buf)).unwrap();
        assert!(matches!(
            r.section(SectionKind::GeometryIndex),
            Err(ArtifactError::NotImplemented { .. })
        ));
    }
}

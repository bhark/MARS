//! section header read/write helpers. on-wire layout per FORMAT.md.

use crate::{ArtifactError, SectionKind};

pub(crate) const SECTION_HEADER_LEN: usize = 2 + 2 + 8;

pub(crate) const FLAG_COMPRESSED: u16 = 0x0001;

#[derive(Debug, Clone, Copy)]
pub(crate) struct SectionHeader {
    pub kind: u16,
    pub flags: u16,
    pub length: u64,
}

impl SectionHeader {
    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.kind.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&self.length.to_le_bytes());
    }

    pub(crate) fn read(bytes: &[u8]) -> Result<Self, ArtifactError> {
        if bytes.len() < SECTION_HEADER_LEN {
            return Err(ArtifactError::Truncated);
        }
        let kind = u16::from_le_bytes([bytes[0], bytes[1]]);
        let flags = u16::from_le_bytes([bytes[2], bytes[3]]);
        let length = u64::from_le_bytes([
            bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11],
        ]);
        Ok(Self { kind, flags, length })
    }
}

impl SectionKind {
    #[must_use]
    pub fn as_u16(self) -> u16 {
        self as u16
    }
}

//! artifact reader: validates header + footer, exposes section access by kind.

use bytes::Bytes;
use mars_types::{Bbox, ContentHash};

use crate::{
    ArtifactError, ArtifactKind, FORMAT_VERSION, MAGIC, SectionKind, attrs::AttributesSection,
    generated::mars::artifact as fb, section::FLAG_COMPRESSED, writer::SourceRef,
};

const HEADER_LEN: usize = 8 + 4 + 4;
const TRAILER_LEN: usize = 4 + 8;

#[derive(Debug, Clone)]
pub struct ArtifactReader {
    bytes: Bytes,
    kind: ArtifactKind,
    bbox: Bbox,
    feature_count: u64,
    sections: Vec<SectionIndexEntry>,
    source_ref: Option<SourceRef>,
}

#[derive(Debug, Clone, Copy)]
struct SectionIndexEntry {
    kind: u16,
    file_offset: u64,
    length: u64,
}

impl ArtifactReader {
    pub fn open(bytes: Bytes) -> Result<Self, ArtifactError> {
        if bytes.len() < HEADER_LEN + TRAILER_LEN {
            return Err(ArtifactError::Truncated);
        }
        if &bytes[..MAGIC.len()] != MAGIC {
            return Err(ArtifactError::BadMagic);
        }
        let version = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        if version != FORMAT_VERSION {
            return Err(ArtifactError::UnsupportedVersion(version));
        }
        // header flags bytes 12..16 currently reserved/unused

        // trailer
        let trailer_off = bytes.len() - TRAILER_LEN;
        let footer_len = u32::from_le_bytes(
            bytes[trailer_off..trailer_off + 4]
                .try_into()
                .map_err(|_| ArtifactError::Truncated)?,
        ) as usize;
        if &bytes[trailer_off + 4..] != MAGIC {
            return Err(ArtifactError::BadMagic);
        }
        let footer_end = trailer_off;
        let footer_start = footer_end.checked_sub(footer_len).ok_or(ArtifactError::Truncated)?;
        if footer_start < HEADER_LEN {
            return Err(ArtifactError::Truncated);
        }
        let footer_bytes = &bytes[footer_start..footer_end];
        let footer =
            planus::ReadAsRoot::read_as_root(footer_bytes).map_err(|_| ArtifactError::Malformed("footer parse"))?;
        let footer: fb::FooterRef<'_> = footer;

        let kind = ArtifactKind::try_from(footer.kind().map_err(|_| ArtifactError::Malformed("footer kind"))?)?;

        let bbox_raw = footer
            .bbox()
            .map_err(|_| ArtifactError::Malformed("footer bbox"))?
            .ok_or(ArtifactError::Malformed("footer bbox missing"))?;
        let bbox = Bbox::new(bbox_raw.min_x(), bbox_raw.min_y(), bbox_raw.max_x(), bbox_raw.max_y());

        let feature_count = footer
            .feature_count()
            .map_err(|_| ArtifactError::Malformed("footer feature_count"))?;

        let sections_vec = footer
            .sections()
            .map_err(|_| ArtifactError::Malformed("footer sections"))?;
        let mut sections = Vec::new();
        if let Some(list) = sections_vec {
            for entry in list.iter() {
                let kind = entry.kind();
                // duplicate kinds would be silently shadowed by `section()`'s
                // first-match lookup; reject up front.
                if sections.iter().any(|s: &SectionIndexEntry| s.kind == kind) {
                    return Err(ArtifactError::DuplicateSection(kind));
                }
                sections.push(SectionIndexEntry {
                    kind,
                    file_offset: entry.file_offset(),
                    length: entry.length(),
                });
            }
        }

        let source_ref = match footer
            .source_artifact_ref()
            .map_err(|_| ArtifactError::Malformed("footer source_ref"))?
        {
            Some(s) => {
                let coll = s
                    .collection()
                    .map_err(|_| ArtifactError::Malformed("source_ref collection"))?
                    .ok_or(ArtifactError::Malformed("source_ref collection missing"))?
                    .to_owned();
                let band = s
                    .band()
                    .map_err(|_| ArtifactError::Malformed("source_ref band"))?
                    .ok_or(ArtifactError::Malformed("source_ref band missing"))?
                    .to_owned();
                let cell_x = s.cell_x().map_err(|_| ArtifactError::Malformed("source_ref cell_x"))?;
                let cell_y = s.cell_y().map_err(|_| ArtifactError::Malformed("source_ref cell_y"))?;
                let hash_slice = s
                    .content_hash()
                    .map_err(|_| ArtifactError::Malformed("source_ref content_hash"))?
                    .ok_or(ArtifactError::Malformed("source_ref content_hash missing"))?;
                let hash_arr: [u8; 32] = hash_slice
                    .try_into()
                    .map_err(|_| ArtifactError::Malformed("source_ref content_hash length"))?;
                Some(SourceRef {
                    collection: coll,
                    band,
                    cell_x,
                    cell_y,
                    content_hash: ContentHash(hash_arr),
                })
            }
            None => None,
        };

        // sanity: each indexed section lies wholly inside the section area
        for s in &sections {
            let end = s
                .file_offset
                .checked_add(s.length)
                .ok_or(ArtifactError::Malformed("section span overflow"))?;
            let file_offset: usize = s
                .file_offset
                .try_into()
                .map_err(|_| ArtifactError::Malformed("file_offset too large"))?;
            let end_usize: usize = end
                .try_into()
                .map_err(|_| ArtifactError::Malformed("section end too large"))?;
            if file_offset < HEADER_LEN || end_usize > footer_start {
                return Err(ArtifactError::Malformed("section out of range"));
            }
        }

        // mirror writer's invariant: the geometry payload's leading u32 must
        // match footer.feature_count. cheap, catches malformed/forged blobs.
        let geom_target = SectionKind::GeometryPayload.as_u16();
        if let Some(s) = sections.iter().find(|e| e.kind == geom_target) {
            let file_offset: usize = s
                .file_offset
                .try_into()
                .map_err(|_| ArtifactError::Malformed("section offset too large"))?;
            if s.length < 4 || bytes.len() < file_offset + 4 {
                return Err(ArtifactError::Truncated);
            }
            let payload_count = u32::from_le_bytes([
                bytes[file_offset],
                bytes[file_offset + 1],
                bytes[file_offset + 2],
                bytes[file_offset + 3],
            ]) as u64;
            if payload_count != feature_count {
                return Err(ArtifactError::Malformed("feature_count mismatch"));
            }
        }

        Ok(Self {
            bytes,
            kind,
            bbox,
            feature_count,
            sections,
            source_ref,
        })
    }

    #[must_use]
    pub fn kind(&self) -> ArtifactKind {
        self.kind
    }

    #[must_use]
    pub fn bbox(&self) -> Bbox {
        self.bbox
    }

    #[must_use]
    pub fn feature_count(&self) -> u64 {
        self.feature_count
    }

    #[must_use]
    pub fn source_ref(&self) -> Option<&SourceRef> {
        self.source_ref.as_ref()
    }

    /// Borrow the attributes section, validated against the v3 directory format.
    /// Returns `Err(SectionMissing)` if no Attributes section is present.
    pub fn attributes_section(&self) -> Result<AttributesSection<'_>, ArtifactError> {
        let bytes = self.section_slice(SectionKind::Attributes)?;
        Ok(AttributesSection::open(bytes)?)
    }

    /// Random-access lookup of one row's per-feature payload by `feature_id`.
    /// Returns `Ok(None)` when the id is absent from the attributes section.
    /// Errors out if the section is missing or malformed.
    pub fn attributes_by_feature_id(&self, feature_id: u64) -> Result<Option<&[u8]>, ArtifactError> {
        let sec = self.attributes_section()?;
        Ok(sec.lookup(feature_id)?)
    }

    /// Borrow a section payload by kind without copying. Used by typed
    /// accessors such as [`Self::attributes_section`].
    fn section_slice(&self, kind: SectionKind) -> Result<&[u8], ArtifactError> {
        let target = kind.as_u16();
        let entry = self
            .sections
            .iter()
            .find(|e| e.kind == target)
            .ok_or(ArtifactError::SectionMissing(kind))?;
        let file_offset: usize = entry
            .file_offset
            .try_into()
            .map_err(|_| ArtifactError::Malformed("section offset too large"))?;
        let length: usize = entry
            .length
            .try_into()
            .map_err(|_| ArtifactError::Malformed("section length too large"))?;
        let end = file_offset
            .checked_add(length)
            .ok_or(ArtifactError::Malformed("section span overflow"))?;
        Ok(&self.bytes[file_offset..end])
    }

    /// zero-copy slice of an uncompressed section payload by kind. v1 rejects
    /// any section flagged compressed (reserved for phase 1).
    pub fn section(&self, kind: SectionKind) -> Result<Bytes, ArtifactError> {
        let target = kind.as_u16();
        let entry = self
            .sections
            .iter()
            .find(|e| e.kind == target)
            .ok_or(ArtifactError::SectionMissing(kind))?;

        // re-read the section header to enforce compressed-flag check
        let file_offset: usize = entry
            .file_offset
            .try_into()
            .map_err(|_| ArtifactError::Malformed("section offset too large"))?;
        let hdr_off = file_offset
            .checked_sub(crate::section::SECTION_HEADER_LEN)
            .ok_or(ArtifactError::Malformed("section header underflow"))?;
        let hdr = crate::section::SectionHeader::read(&self.bytes[hdr_off..])?;
        if hdr.kind != target {
            return Err(ArtifactError::Malformed("section kind mismatch"));
        }
        if hdr.flags & FLAG_COMPRESSED != 0 {
            return Err(ArtifactError::CompressedNotSupported);
        }

        let length: usize = entry
            .length
            .try_into()
            .map_err(|_| ArtifactError::Malformed("section length too large"))?;
        let start = file_offset;
        let end = start
            .checked_add(length)
            .ok_or(ArtifactError::Malformed("section span overflow"))?;
        Ok(self.bytes.slice(start..end))
    }
}

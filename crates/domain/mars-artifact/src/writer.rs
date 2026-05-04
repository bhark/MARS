//! artifact writer. builds the in-memory byte buffer, no i/o.

use bytes::Bytes;
use mars_types::{Bbox, ContentHash};

use crate::{
    ArtifactError, ArtifactKind, FORMAT_VERSION, MAGIC, SectionKind, class_assignment,
    generated::mars::artifact as fb,
    geometry::{self, FeatureGeom},
    section::SectionHeader,
    style_refs,
};

#[derive(Debug, Clone)]
pub struct SourceRef {
    pub collection: String,
    pub band: String,
    pub cell_x: i64,
    pub cell_y: i64,
    pub content_hash: ContentHash,
}

#[derive(Debug)]
pub struct ArtifactWriter {
    kind: ArtifactKind,
    sections: Vec<(SectionKind, Bytes)>,
    bbox: Bbox,
    feature_count: u64,
    source_ref: Option<SourceRef>,
}

impl ArtifactWriter {
    #[must_use]
    pub fn new(kind: ArtifactKind) -> Self {
        Self {
            kind,
            sections: Vec::new(),
            bbox: Bbox::new(0.0, 0.0, 0.0, 0.0),
            feature_count: 0,
            source_ref: None,
        }
    }

    pub fn add_section(&mut self, kind: SectionKind, payload: Bytes) -> &mut Self {
        self.sections.push((kind, payload));
        self
    }

    pub fn add_geometry_payload(&mut self, features: &[FeatureGeom]) -> &mut Self {
        let bytes = geometry::encode_geometry_payload(features);
        self.add_section(SectionKind::GeometryPayload, bytes)
    }

    pub fn add_class_assignment(&mut self, items: &[(u64, u16)]) -> &mut Self {
        let bytes = class_assignment::encode_class_assignment(items);
        self.add_section(SectionKind::ClassAssignment, bytes)
    }

    pub fn add_style_refs(&mut self, refs: &[String]) -> &mut Self {
        let bytes = style_refs::encode_style_refs(refs);
        self.add_section(SectionKind::StyleRefs, bytes)
    }

    pub fn set_bbox(&mut self, bbox: Bbox) -> &mut Self {
        self.bbox = bbox;
        self
    }

    pub fn set_feature_count(&mut self, n: u64) -> &mut Self {
        self.feature_count = n;
        self
    }

    pub fn set_source_ref(&mut self, sref: SourceRef) -> &mut Self {
        self.source_ref = Some(sref);
        self
    }

    /// finalize and produce the artifact bytes. determinism: identical inputs
    /// yield byte-identical output (planus serializes tables in a fixed order).
    pub fn finish(self) -> Result<Bytes, ArtifactError> {
        let mut out = Vec::new();
        // header
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // flags

        // sections + capture (kind, file_offset, length) for footer index
        let mut entries: Vec<(u16, u64, u64)> = Vec::with_capacity(self.sections.len());
        for (kind, payload) in &self.sections {
            let header = SectionHeader {
                kind: kind.as_u16(),
                flags: 0,
                length: payload.len() as u64,
            };
            let payload_offset = (out.len() + crate::section::SECTION_HEADER_LEN) as u64;
            header.write(&mut out);
            entries.push((kind.as_u16(), payload_offset, payload.len() as u64));
            out.extend_from_slice(payload);
        }

        // build footer
        let footer_table = fb::Footer {
            kind: self.kind.into(),
            sections: Some(
                entries
                    .iter()
                    .map(|(k, off, len)| fb::SectionEntry {
                        kind: *k,
                        file_offset: *off,
                        length: *len,
                        uncompressed_length: *len,
                    })
                    .collect(),
            ),
            bbox: Some(fb::Bbox {
                min_x: self.bbox.min_x,
                min_y: self.bbox.min_y,
                max_x: self.bbox.max_x,
                max_y: self.bbox.max_y,
            }),
            feature_count: self.feature_count,
            source_artifact_ref: self.source_ref.as_ref().map(|s| {
                Box::new(fb::SourceRef {
                    collection: Some(s.collection.clone()),
                    band: Some(s.band.clone()),
                    cell_x: s.cell_x,
                    cell_y: s.cell_y,
                    content_hash: Some(s.content_hash.0.to_vec()),
                })
            }),
        };

        let mut builder = planus::Builder::new();
        let footer_bytes = builder.finish(&footer_table, None);
        let footer_len: u32 = footer_bytes
            .len()
            .try_into()
            .map_err(|_| ArtifactError::Malformed("footer too large"))?;
        out.extend_from_slice(footer_bytes);
        out.extend_from_slice(&footer_len.to_le_bytes());
        out.extend_from_slice(MAGIC);

        Ok(Bytes::from(out))
    }
}

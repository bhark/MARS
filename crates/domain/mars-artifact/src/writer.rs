//! artifact writer. builds the in-memory byte buffer, no i/o.

use bytes::Bytes;
use mars_types::{Bbox, ContentHash};

use crate::{
    ArtifactError, ArtifactKind, FORMAT_VERSION, MAGIC, SectionKind, class_assignment,
    generated::mars::artifact as fb,
    geometry::{self, FeatureGeom},
    label_candidates::{self, LabelCandidate},
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
    bbox: Option<Bbox>,
    feature_count: Option<u64>,
    source_ref: Option<SourceRef>,
    // deferred so finish() can validate (ascending feature_id, declared count)
    // and surface errors that the infallible add_* calls cannot.
    pending_features: Option<Vec<FeatureGeom>>,
    pending_class_assignment: Option<Vec<(u64, u16)>>,
    pending_label_candidates: Option<Vec<LabelCandidate>>,
}

impl ArtifactWriter {
    #[must_use]
    pub fn new(kind: ArtifactKind) -> Self {
        Self {
            kind,
            sections: Vec::new(),
            bbox: None,
            feature_count: None,
            source_ref: None,
            pending_features: None,
            pending_class_assignment: None,
            pending_label_candidates: None,
        }
    }

    /// Append a raw, pre-encoded section. Duplicate-kind checks (including
    /// against the typed staging helpers for geometry/class/label) run in
    /// `finish()`; the builder API stays infallible here.
    pub fn add_section(&mut self, kind: SectionKind, payload: Bytes) -> &mut Self {
        self.sections.push((kind, payload));
        self
    }

    /// Stage geometry features. Encoding (and any errors) is deferred to
    /// `finish()` so that the builder API stays uniformly infallible. Takes
    /// ownership to avoid an unnecessary clone of what is often a large vec.
    pub fn add_geometry_payload(&mut self, features: Vec<FeatureGeom>) -> &mut Self {
        self.pending_features = Some(features);
        self
    }

    pub fn add_class_assignment(&mut self, items: &[(u64, u16)]) -> &mut Self {
        self.pending_class_assignment = Some(items.to_vec());
        self
    }

    pub fn add_label_candidates(&mut self, items: &[LabelCandidate]) -> &mut Self {
        self.pending_label_candidates = Some(items.to_vec());
        self
    }

    pub fn add_style_refs(&mut self, refs: &[String]) -> &mut Self {
        let bytes = style_refs::encode_style_refs(refs);
        self.add_section(SectionKind::StyleRefs, bytes)
    }

    pub fn set_bbox(&mut self, bbox: Bbox) -> &mut Self {
        self.bbox = Some(bbox);
        self
    }

    pub fn set_feature_count(&mut self, n: u64) -> &mut Self {
        self.feature_count = Some(n);
        self
    }

    pub fn set_source_ref(&mut self, sref: SourceRef) -> &mut Self {
        self.source_ref = Some(sref);
        self
    }

    /// finalize and produce the artifact bytes. determinism: identical inputs
    /// yield byte-identical output (planus serializes tables in a fixed order).
    ///
    /// All cross-field invariants are validated here:
    /// - bbox must have been set
    /// - source artifacts must not carry a source_ref (SPEC §9.2)
    /// - feature_count, when present alongside geometry, must equal the
    ///   actual feature count encoded in the payload
    pub fn finish(mut self) -> Result<Bytes, ArtifactError> {
        let bbox = self.bbox.ok_or(ArtifactError::InvalidWriterState("bbox not set"))?;

        if matches!(self.kind, ArtifactKind::Source) && self.source_ref.is_some() {
            return Err(ArtifactError::InvalidWriterState(
                "source artifacts must not carry a source_ref",
            ));
        }

        // resolve pending geometry now: the encoder both validates input
        // ordering and lets us cross-check feature_count.
        if let Some(features) = self.pending_features.take() {
            if let Some(declared) = self.feature_count
                && declared != features.len() as u64
            {
                return Err(ArtifactError::InvalidWriterState(
                    "feature_count does not match encoded feature count",
                ));
            }
            let bytes = geometry::encode_geometry_payload(&features)?;
            self.sections.push((SectionKind::GeometryPayload, bytes));
        }
        if let Some(items) = self.pending_class_assignment.take() {
            let bytes = class_assignment::encode_class_assignment(&items)?;
            self.sections.push((SectionKind::ClassAssignment, bytes));
        }
        if let Some(items) = self.pending_label_candidates.take() {
            let bytes = label_candidates::encode_label_candidates(&items)?;
            self.sections.push((SectionKind::LabelCandidates, bytes));
        }

        // reject duplicate sections (e.g. caller staged geometry via
        // add_geometry_payload and also pre-encoded one through add_section).
        // ArtifactReader::open already errors on duplicates; catching it here
        // gives the encoder side a clear error instead of producing a blob
        // that fails to open.
        for i in 0..self.sections.len() {
            for j in (i + 1)..self.sections.len() {
                if self.sections[i].0 == self.sections[j].0 {
                    return Err(ArtifactError::DuplicateSection(self.sections[i].0.as_u16()));
                }
            }
        }

        let feature_count = self.feature_count.unwrap_or(0);

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
                min_x: bbox.min_x,
                min_y: bbox.min_y,
                max_x: bbox.max_x,
                max_y: bbox.max_y,
            }),
            feature_count,
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

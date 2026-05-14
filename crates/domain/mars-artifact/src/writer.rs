//! artifact writer. builds the in-memory byte buffer, no i/o.

use bytes::Bytes;
use mars_types::{Bbox, ContentHash};

use crate::{
    ArtifactError, ArtifactKind, FORMAT_VERSION, MAGIC, SectionKind, attrs, class_assignment,
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
    // sections are stored as raw u16 kinds so additive forward-compat tests
    // (and any future codec extension) can inject section kinds the current
    // SectionKind enum does not yet name. typed add_* helpers convert.
    sections: Vec<(u16, Bytes)>,
    bbox: Option<Bbox>,
    feature_count: Option<u64>,
    source_ref: Option<SourceRef>,
    // deferred so finish() can validate (ascending feature_id, declared count)
    // and surface errors that the infallible add_* calls cannot.
    pending_features: Option<Vec<FeatureGeom>>,
    pending_class_assignment: Option<Vec<(u32, u16)>>,
    pending_label_candidates: Option<Vec<LabelCandidate>>,
    pending_attributes: Option<Vec<(u32, Vec<u8>)>>,
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
            pending_attributes: None,
        }
    }

    /// Append a raw, pre-encoded section. Duplicate-kind checks (including
    /// against the typed staging helpers for geometry/class/label) run in
    /// `finish()`; the builder API stays infallible here.
    pub fn add_section(&mut self, kind: SectionKind, payload: Bytes) -> &mut Self {
        self.sections.push((kind.as_u16(), payload));
        self
    }

    /// Append a section with a raw `u16` kind. Used by forward-compat tests
    /// that exercise readers' tolerance of section kinds the enum does not
    /// yet name. Production code goes through [`Self::add_section`].
    #[cfg(test)]
    pub(crate) fn add_raw_section(&mut self, kind: u16, payload: Bytes) -> &mut Self {
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

    pub fn add_class_assignment(&mut self, items: &[(u32, u16)]) -> &mut Self {
        self.pending_class_assignment = Some(items.to_vec());
        self
    }

    /// Stage an attributes section. Each `(feature_idx, row_bytes)` pair is
    /// the per-feature payload produced by [`attrs::encode_row`] keyed on the
    /// page-local slot index; the writer wraps them in a directory-indexed
    /// section so the reader can resolve by slot. Errors surface in
    /// [`Self::finish`].
    pub fn add_attributes(&mut self, rows: Vec<(u32, Vec<u8>)>) -> &mut Self {
        self.pending_attributes = Some(rows);
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

    /// attaches a pre-built packed hilbert r-tree (see `SpatialIndexBuilder`).
    pub fn add_spatial_index(&mut self, payload: Bytes) -> &mut Self {
        self.add_section(SectionKind::SpatialIndex, payload)
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
    /// - source artifacts must not carry a source_ref
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
        // ordering and lets us cross-check feature_count. derive the count
        // from the staged features when the caller didn't set one.
        if let Some(features) = self.pending_features.take() {
            let actual = features.len() as u64;
            match self.feature_count {
                Some(declared) if declared != actual => {
                    return Err(ArtifactError::InvalidWriterState(
                        "feature_count does not match encoded feature count",
                    ));
                }
                None => self.feature_count = Some(actual),
                _ => {}
            }
            let bytes = geometry::encode_geometry_payload(&features)?;
            self.sections.push((SectionKind::GeometryPayload.as_u16(), bytes));
        }
        if let Some(items) = self.pending_class_assignment.take() {
            let bytes = class_assignment::encode_class_assignment(&items)?;
            self.sections.push((SectionKind::ClassAssignment.as_u16(), bytes));
        }
        if let Some(items) = self.pending_label_candidates.take() {
            let bytes = label_candidates::encode_label_candidates(&items)?;
            self.sections.push((SectionKind::LabelCandidates.as_u16(), bytes));
        }
        if let Some(rows) = self.pending_attributes.take() {
            let refs: Vec<(u32, &[u8])> = rows.iter().map(|(idx, p)| (*idx, p.as_slice())).collect();
            let bytes = attrs::encode_attributes_section(&refs)?;
            self.sections.push((SectionKind::Attributes.as_u16(), bytes));
        }

        // reject duplicate sections (e.g. caller staged geometry via
        // add_geometry_payload and also pre-encoded one through add_section).
        // ArtifactReader::open already errors on duplicates; catching it here
        // gives the encoder side a clear error instead of producing a blob
        // that fails to open.
        for i in 0..self.sections.len() {
            for j in (i + 1)..self.sections.len() {
                if self.sections[i].0 == self.sections[j].0 {
                    return Err(ArtifactError::DuplicateSection(self.sections[i].0));
                }
            }
        }

        // a geometry section without a known feature_count would silently lie
        // in the footer (zero features, body has them). require it explicitly
        // when geometry is present and could not be derived from staged input.
        let has_geometry = self
            .sections
            .iter()
            .any(|(k, _)| *k == SectionKind::GeometryPayload.as_u16());
        let feature_count = match self.feature_count {
            Some(n) => n,
            None if has_geometry => {
                return Err(ArtifactError::InvalidWriterState(
                    "feature_count not set for artifact with geometry payload",
                ));
            }
            None => 0,
        };

        let mut out = Vec::new();
        // header
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // flags

        // sections + capture (kind, file_offset, length) for footer index
        let mut entries: Vec<(u16, u64, u64)> = Vec::with_capacity(self.sections.len());
        for (kind, payload) in &self.sections {
            let header = SectionHeader {
                kind: *kind,
                flags: 0,
                length: payload.len() as u64,
            };
            let payload_offset = (out.len() + crate::section::SECTION_HEADER_LEN) as u64;
            header.write(&mut out);
            entries.push((*kind, payload_offset, payload.len() as u64));
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

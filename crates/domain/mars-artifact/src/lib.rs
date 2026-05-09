//! mars artifact container codec. on-disk layout per SPEC §9.3 / FORMAT.md.
//! synchronous codec over `&[u8]` / `bytes::Bytes`; async i/o stays in adapters.

/// MARS magic bytes - also used as the trailer.
pub const MAGIC: &[u8; 8] = b"MARS\0\0\0\0";

/// Format version of the on-disk container. Bumped on incompatible changes.
pub const FORMAT_VERSION: u32 = 2;

// generated planus code uses `unsafe` for zero-copy reads; it is the only
// place we permit unsafe in this crate. all hand-written code below is safe.
#[allow(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    dead_code,
    unused_imports,
    unreachable_pub,
    unsafe_code
)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/generated.rs"));
}

pub mod attrs;
mod class_assignment;
mod geometry;
mod hash;
mod label_candidates;
mod reader;
mod section;
mod spatial_index;
mod style_refs;
mod varint;
mod wkb;
mod writer;

pub use attrs::{
    AttrError, AttrValue, AttributesSection, MAX_ROW_BYTES, decode_row, encode_attributes_section, encode_row,
};
pub use class_assignment::{decode_class_assignment, encode_class_assignment};
pub use geometry::{
    Coord, FeatureGeom, FeatureIndexEntry, FeatureIndexIter, FeatureWriter, GeomKind, GeomPayloadBuilder, GeomType,
    GeomVisitor, decode_geometry_at_slots, decode_geometry_payload, decode_geometry_payload_filtered, decode_one_geom,
    encode_geometry_payload, iter_feature_index, visit_one_geom,
};
pub use hash::compute_content_hash;
pub use label_candidates::{LabelCandidate, LabelShape, decode_label_candidates, encode_label_candidates};
pub use reader::ArtifactReader;
pub use spatial_index::{DEFAULT_NODE_SIZE, SpatialIndex, SpatialIndexBuilder};
pub use style_refs::{decode_style_refs, encode_style_refs};
pub use wkb::{WkbError, wkb_bbox, wkb_centroid, wkb_to_feature_geom};
pub use writer::{ArtifactWriter, SourceRef};

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("not a MARS artifact (bad magic)")]
    BadMagic,
    #[error("unsupported format version {0}")]
    UnsupportedVersion(u32),
    #[error("truncated artifact")]
    Truncated,
    #[error("malformed artifact: {0}")]
    Malformed(&'static str),
    #[error("unknown artifact kind {0}")]
    UnknownKind(u8),
    #[error("section {0:?} not present")]
    SectionMissing(SectionKind),
    #[error("section kind {0:#04x} listed more than once in footer")]
    DuplicateSection(u16),
    #[error("compressed sections are not supported in v1")]
    CompressedNotSupported,
    #[error("invalid writer state: {0}")]
    InvalidWriterState(&'static str),
    #[error("coordinate {0} out of representable range for quantization")]
    CoordOutOfRange(f64),
    #[error("attributes section: {0}")]
    Attrs(#[from] AttrError),
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SectionKind {
    GeometryPayload = 0x02,
    Attributes = 0x03,
    LabelCandidates = 0x04,
    ClassAssignment = 0x05,
    StyleRefs = 0x06,
    SpatialIndex = 0x07,
}

/// artifact role at the container level. mirrors the planus enum but lives in
/// rust as a stable public type so consumers don't import generated code.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtifactKind {
    Source = 0,
    Layer = 1,
    Style = 2,
}

impl From<ArtifactKind> for generated::mars::artifact::ArtifactKind {
    fn from(k: ArtifactKind) -> Self {
        match k {
            ArtifactKind::Source => Self::Source,
            ArtifactKind::Layer => Self::Layer,
            ArtifactKind::Style => Self::Style,
        }
    }
}

impl TryFrom<generated::mars::artifact::ArtifactKind> for ArtifactKind {
    type Error = ArtifactError;

    fn try_from(k: generated::mars::artifact::ArtifactKind) -> Result<Self, Self::Error> {
        use generated::mars::artifact::ArtifactKind as G;
        Ok(match k {
            G::Source => Self::Source,
            G::Layer => Self::Layer,
            G::Style => Self::Style,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;

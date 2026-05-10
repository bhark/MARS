//! geometry payload v1 codec. see FORMAT.md.
//!
//! the 33-byte feature index stride is unaligned, so the decoder copies each
//! field via `from_le_bytes` rather than zero-casting to a typed slice.

mod builder;
mod codec;
mod decode;
mod index;
mod visit;

pub type Coord = (f64, f64);

#[derive(Debug, Clone, PartialEq)]
pub enum GeomKind {
    Point(Coord),
    LineString(Vec<Coord>),
    Polygon(Vec<Vec<Coord>>),
    MultiPoint(Vec<Coord>),
    MultiLineString(Vec<Vec<Coord>>),
    MultiPolygon(Vec<Vec<Vec<Coord>>>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct FeatureGeom {
    /// Source-supplied identifier. Carried as data, not as the substrate's
    /// primary key; non-uniqueness is allowed (a source row exploded into
    /// multiple parts shares the same user_id). The per-page primary key is
    /// the positional slot index (`feature_idx`) assigned at encode time.
    pub user_id: u64,
    /// Per-feature bounding box stored as f32. At canonical-CRS
    /// magnitudes (~6e5 m for Danish UTM-32) this is ~0.05 m of precision,
    /// so the index bbox is APPROXIMATE: feature-level filtering must not
    /// rely on it for sub-meter discrimination - re-test against the decoded
    /// geometry when accuracy matters.
    pub bbox: [f32; 4],
    pub geom: GeomKind,
}

/// Geometry-type tag handed to [`GeomPayloadBuilder::begin`]. Kept separate
/// from the on-wire `u8` so callers can't pass an arbitrary byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeomType {
    Point,
    LineString,
    Polygon,
    MultiPoint,
    MultiLineString,
    MultiPolygon,
}

impl GeomType {
    #[inline]
    pub(crate) fn byte(self) -> u8 {
        match self {
            Self::Point => GT_POINT,
            Self::LineString => GT_LINESTRING,
            Self::Polygon => GT_POLYGON,
            Self::MultiPoint => GT_MULTIPOINT,
            Self::MultiLineString => GT_MULTILINESTRING,
            Self::MultiPolygon => GT_MULTIPOLYGON,
        }
    }
}

pub(crate) const GT_POINT: u8 = 1;
pub(crate) const GT_LINESTRING: u8 = 2;
pub(crate) const GT_POLYGON: u8 = 3;
pub(crate) const GT_MULTIPOINT: u8 = 4;
pub(crate) const GT_MULTILINESTRING: u8 = 5;
pub(crate) const GT_MULTIPOLYGON: u8 = 6;

/// hard limit on coordinates per ring or points per multipoint.
pub(crate) const MAX_GEOM_COORDS: usize = 1_000_000;
/// hard limit on rings / parts / polygons per geometry.
pub(crate) const MAX_GEOM_PARTS: usize = 100_000;

pub use builder::{encode_geometry_payload, FeatureWriter, GeomPayloadBuilder};
pub(crate) use index::FEATURE_INDEX_ENTRY_LEN;
pub use index::{FeatureIndexEntry, FeatureIndexIter, iter_feature_index};
pub use decode::{decode_geometry_at_slots, decode_geometry_payload, decode_geometry_payload_filtered, decode_one_geom};
pub use visit::{GeomVisitor, visit_one_geom};

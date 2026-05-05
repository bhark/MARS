//! core value types shared across MARS. pure data, no i/o, no async.

#![forbid(unsafe_code)]

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// schema version stamped onto layer-artifact keys (SPEC §10.x). Bumped when the
/// layer artifact's payload format changes; compiler and runtime read from here
/// to keep key construction and parsing in lockstep.
pub const LAYER_SCHEMA_VERSION: u32 = 1;

/// current `Manifest::format_version`. Incremented on incompatible additions to
/// the on-disk manifest format. Older readers must reject newer values.
pub const MANIFEST_FORMAT_VERSION: u32 = 2;

/// inclusive bounding box in canonical CRS units.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bbox {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Bbox {
    #[must_use]
    pub const fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        Self {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    #[must_use]
    pub fn width(self) -> f64 {
        self.max_x - self.min_x
    }

    #[must_use]
    pub fn height(self) -> f64 {
        self.max_y - self.min_y
    }
}

/// declares a transparent `String` newtype with the standard accessor surface
/// (`new`, `as_str`), `Display`, `From<&str>`, and serde transparent ser/de.
/// keeps the wire form a plain string while hiding the inner field.
#[macro_export]
macro_rules! impl_string_newtype {
    ($(#[$meta:meta])* $vis:vis $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, ::serde::Serialize, ::serde::Deserialize)]
        #[serde(transparent)]
        $vis struct $name(String);

        impl $name {
            #[must_use]
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self::new(s)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self::new(s)
            }
        }
    };
}

impl_string_newtype!(
    /// scale-band identifier (e.g. `"ultra"`, `"hi"`, `"med"`).
    pub ScaleBand
);

impl_string_newtype!(
    /// CRS authority code, e.g. `EPSG:25832`. dedup axis (SPEC §7).
    pub CrsCode
);

impl_string_newtype!(
    /// stable layer identifier inside a service. dedup axis (SPEC §7).
    pub LayerId
);

impl_string_newtype!(
    /// object-store key for an artifact. dedup axis (SPEC §7).
    pub ArtifactKey
);

impl_string_newtype!(
    /// per-request id, propagated end-to-end through tracing spans.
    pub RequestId
);

/// a spatial partition cell, addressed by `(band, x, y)` in canonical CRS.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Cell {
    pub band: ScaleBand,
    pub x: i64,
    pub y: i64,
}

/// 32-byte content hash (BLAKE3) used as physical artifact addressing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentHash(pub [u8; 32]);

impl ContentHash {
    #[must_use]
    pub const fn zero() -> Self {
        Self([0u8; 32])
    }

    /// lowercase hex (64 chars). Matches the `{hash}.mars` segment in keys.
    #[must_use]
    pub fn to_hex(&self) -> String {
        use core::fmt::Write;
        let mut s = String::with_capacity(64);
        for b in &self.0 {
            // infallible: pre-allocated string
            let _ = write!(s, "{b:02x}");
        }
        s
    }
}

impl core::fmt::Display for ContentHash {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl ArtifactKey {
    /// canonical layer-artifact key. compiler builds; runtime parses with
    /// [`Self::parse`]. shape is `lyr/{layer}/{band}/{cx}_{cy}/v{schema}/{hash}.mars`.
    #[must_use]
    pub fn build_layer(layer: &LayerId, cell: &Cell, hash: ContentHash) -> Self {
        Self::new(format!(
            "lyr/{layer}/{band}/{cx}_{cy}/v{ver}/{hex}.mars",
            layer = layer.as_str(),
            band = cell.band.as_str(),
            cx = cell.x,
            cy = cell.y,
            ver = LAYER_SCHEMA_VERSION,
            hex = hash.to_hex(),
        ))
    }

    /// canonical source-artifact key. shape is
    /// `src/{collection}/{band}/{cx}_{cy}/{hash}.mars`.
    #[must_use]
    pub fn build_source(collection: &str, cell: &Cell, hash: ContentHash) -> Self {
        Self::new(format!(
            "src/{collection}/{band}/{cx}_{cy}/{hex}.mars",
            band = cell.band.as_str(),
            cx = cell.x,
            cy = cell.y,
            hex = hash.to_hex(),
        ))
    }

    /// parse a manifest key into its semantic shape. compiler and runtime use
    /// the same code path, so a key the compiler writes is, by construction, a
    /// key the runtime parses.
    pub fn parse(&self) -> Result<ParsedArtifactKey, ArtifactKeyError> {
        let s = self.as_str();
        let parts: Vec<&str> = s.split('/').collect();
        let bad = || ArtifactKeyError::Malformed { key: s.to_owned() };
        match parts.as_slice() {
            ["lyr", layer, band, cell, vseg, leaf] => {
                if !vseg.starts_with('v') || !leaf.ends_with(".mars") {
                    return Err(bad());
                }
                let (cx, cy) = parse_cell_xy(cell).ok_or_else(bad)?;
                Ok(ParsedArtifactKey::Layer {
                    layer: LayerId::new((*layer).to_owned()),
                    cell: Cell {
                        band: ScaleBand::new((*band).to_owned()),
                        x: cx,
                        y: cy,
                    },
                })
            }
            ["src", coll, band, cell, leaf] => {
                if !leaf.ends_with(".mars") {
                    return Err(bad());
                }
                let (cx, cy) = parse_cell_xy(cell).ok_or_else(bad)?;
                Ok(ParsedArtifactKey::Source {
                    collection: (*coll).to_owned(),
                    cell: Cell {
                        band: ScaleBand::new((*band).to_owned()),
                        x: cx,
                        y: cy,
                    },
                })
            }
            _ => Err(bad()),
        }
    }
}

fn parse_cell_xy(seg: &str) -> Option<(i64, i64)> {
    let (x, y) = seg.split_once('_')?;
    Some((x.parse().ok()?, y.parse().ok()?))
}

/// a manifest key parsed back into its semantic shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedArtifactKey {
    Layer { layer: LayerId, cell: Cell },
    Source { collection: String, cell: Cell },
}

/// errors raised while parsing an [`ArtifactKey`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ArtifactKeyError {
    #[error("malformed artifact key '{key}'")]
    Malformed { key: String },
}

/// raster image format the renderer encodes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ImageFormat {
    Png,
    Jpeg,
}

impl ImageFormat {
    /// MIME type string for HTTP `Content-Type` headers.
    #[must_use]
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
        }
    }
}

/// marker for a (layer, cell) that falls inside the layer's published domain
/// but contains no features. emitted by the compiler so the runtime can
/// distinguish "empty by design" from "manifest broken / incomplete".
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EmptyLayerCell {
    pub layer: LayerId,
    pub cell: Cell,
}

fn default_manifest_format_version() -> u32 {
    MANIFEST_FORMAT_VERSION
}

fn default_manifest_created_at() -> SystemTime {
    SystemTime::UNIX_EPOCH
}

/// manifest data-transfer object. SPEC §8.5 / §9.2.
///
/// `format_version` is bumped on incompatible additions to this struct; readers
/// reject unknown values. `created_at` records publication wall-clock time;
/// older manifests without the field default to `UNIX_EPOCH` for back-compat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// On-disk format version of this manifest envelope.
    #[serde(default = "default_manifest_format_version")]
    pub format_version: u32,
    pub version: u64,
    pub service: String,
    /// publication wall-clock time. SystemTime to avoid pulling chrono into
    /// the workspace; serde encodes as `{ secs_since_epoch, nanos_since_epoch }`.
    #[serde(default = "default_manifest_created_at")]
    pub created_at: SystemTime,
    pub source_artifacts: Vec<ArtifactEntry>,
    pub layer_artifacts: Vec<ArtifactEntry>,
    pub style_artifact: Option<ArtifactEntry>,
    /// cells explicitly known to be empty (no features). default = empty vec so
    /// v1 manifests without the field load cleanly during rollback windows.
    #[serde(default)]
    pub empty_layer_cells: Vec<EmptyLayerCell>,
}

impl Manifest {
    /// build a manifest at the current `MANIFEST_FORMAT_VERSION` and `now`.
    #[must_use]
    pub fn new(
        version: u64,
        service: impl Into<String>,
        source_artifacts: Vec<ArtifactEntry>,
        layer_artifacts: Vec<ArtifactEntry>,
        style_artifact: Option<ArtifactEntry>,
        empty_layer_cells: Vec<EmptyLayerCell>,
    ) -> Self {
        Self {
            format_version: MANIFEST_FORMAT_VERSION,
            version,
            service: service.into(),
            created_at: SystemTime::now(),
            source_artifacts,
            layer_artifacts,
            style_artifact,
            empty_layer_cells,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactEntry {
    pub key: ArtifactKey,
    pub hash: ContentHash,
    pub size_bytes: u64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn bbox_dimensions() {
        let b = Bbox::new(0.0, 0.0, 10.0, 5.0);
        assert!((b.width() - 10.0).abs() < f64::EPSILON);
        assert!((b.height() - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn manifest_roundtrip() {
        let m = Manifest::new(1, "demo", vec![], vec![], None, vec![]);
        let s = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn manifest_roundtrip_with_empty_cells() {
        let m = Manifest::new(
            1,
            "demo",
            vec![],
            vec![],
            None,
            vec![EmptyLayerCell {
                layer: LayerId::new("roads"),
                cell: Cell {
                    band: ScaleBand::new("hi"),
                    x: 0,
                    y: 0,
                },
            }],
        );
        let s = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
        assert_eq!(back.empty_layer_cells.len(), 1);
    }

    #[test]
    fn manifest_back_compat_v1_without_empty_cells() {
        // v1 on-disk manifest without empty_layer_cells must load with an empty vec.
        let s = r#"{"version":7,"service":"x","source_artifacts":[],"layer_artifacts":[],"style_artifact":null}"#;
        let m: Manifest = serde_json::from_str(s).unwrap();
        assert_eq!(m.version, 7);
        assert_eq!(m.format_version, MANIFEST_FORMAT_VERSION);
        assert_eq!(m.created_at, SystemTime::UNIX_EPOCH);
        assert!(m.empty_layer_cells.is_empty());
    }

    #[test]
    fn manifest_back_compat_without_optional_fields() {
        // legacy on-disk manifest without format_version / created_at must load.
        let s = r#"{"version":7,"service":"x","source_artifacts":[],"layer_artifacts":[],"style_artifact":null}"#;
        let m: Manifest = serde_json::from_str(s).unwrap();
        assert_eq!(m.version, 7);
        assert_eq!(m.format_version, MANIFEST_FORMAT_VERSION);
        assert_eq!(m.created_at, SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn image_format_mime() {
        assert_eq!(ImageFormat::Png.mime(), "image/png");
        assert_eq!(ImageFormat::Jpeg.mime(), "image/jpeg");
    }

    #[test]
    fn newtype_serde_is_transparent() {
        let l = LayerId::new("parcels");
        let s = serde_json::to_string(&l).unwrap();
        assert_eq!(s, "\"parcels\"");
        let back: LayerId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, l);
    }

    #[test]
    fn artifact_key_layer_roundtrip() {
        let layer = LayerId::new("parcels");
        let cell = Cell {
            band: ScaleBand::new("hi"),
            x: 3,
            y: -2,
        };
        let hash = ContentHash([0xab; 32]);
        let key = ArtifactKey::build_layer(&layer, &cell, hash);
        match key.parse().unwrap() {
            ParsedArtifactKey::Layer { layer: l, cell: c } => {
                assert_eq!(l, layer);
                assert_eq!(c, cell);
            }
            ParsedArtifactKey::Source { .. } => panic!("expected layer"),
        }
    }

    #[test]
    fn artifact_key_source_roundtrip() {
        let cell = Cell {
            band: ScaleBand::new("hi"),
            x: 1,
            y: 2,
        };
        let hash = ContentHash([0x10; 32]);
        let key = ArtifactKey::build_source("buildings", &cell, hash);
        match key.parse().unwrap() {
            ParsedArtifactKey::Source { collection, cell: c } => {
                assert_eq!(collection, "buildings");
                assert_eq!(c, cell);
            }
            ParsedArtifactKey::Layer { .. } => panic!("expected source"),
        }
    }

    #[test]
    fn artifact_key_rejects_malformed() {
        assert!(ArtifactKey::new("nope").parse().is_err());
        assert!(ArtifactKey::new("lyr/x/y/3_z/v1/a.mars").parse().is_err());
        assert!(ArtifactKey::new("lyr/x/y/3_4/x1/a.mars").parse().is_err());
    }

    #[test]
    fn content_hash_display_is_hex() {
        let h = ContentHash([0xab; 32]);
        let s = h.to_string();
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(s, h.to_hex());
    }
}

//! core value types shared across MARS. pure data, no i/o, no async.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

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

/// scale-band identifier (e.g. `"ultra"`, `"hi"`, `"med"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScaleBand(String);

impl ScaleBand {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

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
}

/// per-request id, propagated end-to-end through tracing spans.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestId(pub String);

/// CRS authority code, e.g. `EPSG:25832`. dedup axis (SPEC §7).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CrsCode(String);

impl CrsCode {
    #[must_use]
    pub fn new(code: impl Into<String>) -> Self {
        Self(code.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CrsCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// stable layer identifier inside a service. dedup axis (SPEC §7).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LayerId(String);

impl LayerId {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for LayerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// object-store key for an artifact. dedup axis (SPEC §7).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactKey(String);

impl ArtifactKey {
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ArtifactKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
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

/// manifest data-transfer object. SPEC §8.5 / §9.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u64,
    pub service: String,
    pub source_artifacts: Vec<ArtifactEntry>,
    pub layer_artifacts: Vec<ArtifactEntry>,
    pub style_artifact: Option<ArtifactEntry>,
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
        let m = Manifest {
            version: 1,
            service: "demo".into(),
            source_artifacts: vec![],
            layer_artifacts: vec![],
            style_artifact: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn image_format_mime() {
        assert_eq!(ImageFormat::Png.mime(), "image/png");
        assert_eq!(ImageFormat::Jpeg.mime(), "image/jpeg");
    }
}

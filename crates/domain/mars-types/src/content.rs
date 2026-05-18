//! content addressing primitives and image format metadata.

use serde::{Deserialize, Serialize};

use crate::ids::ArtifactKey;

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

/// pointer to one object-store-resident artifact carrying ancillary data
/// (style bundle, page-membership sidecar). page artifacts have richer
/// metadata and live in `PageEntry`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactEntry {
    pub key: ArtifactKey,
    pub hash: ContentHash,
    pub size_bytes: u64,
}

/// raster image format the renderer encodes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Webp,
}

impl ImageFormat {
    /// MIME type string for HTTP `Content-Type` headers.
    #[must_use]
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }

    /// Parse from a wire MIME string. Returns `None` for anything outside
    /// the supported set. Case-sensitive (OGC wire convention) but accepts
    /// the `image/jpg` alias for JPEG.
    #[must_use]
    pub fn from_mime(mime: &str) -> Option<Self> {
        match mime {
            "image/png" => Some(Self::Png),
            "image/jpeg" | "image/jpg" => Some(Self::Jpeg),
            "image/webp" => Some(Self::Webp),
            _ => None,
        }
    }

    /// Parse from a file extension (no leading dot). Returns `None` for
    /// anything outside the supported set. Case-insensitive.
    #[must_use]
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "webp" => Some(Self::Webp),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;

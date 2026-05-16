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
mod tests {
    use super::*;

    #[test]
    fn content_hash_display_is_hex() {
        let h = ContentHash([0xab; 32]);
        let s = h.to_string();
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(s, h.to_hex());
    }

    #[test]
    fn image_format_mime() {
        assert_eq!(ImageFormat::Png.mime(), "image/png");
        assert_eq!(ImageFormat::Jpeg.mime(), "image/jpeg");
        assert_eq!(ImageFormat::Webp.mime(), "image/webp");
    }

    #[test]
    fn image_format_from_mime_round_trips() {
        for fmt in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Webp] {
            assert_eq!(ImageFormat::from_mime(fmt.mime()), Some(fmt));
        }
        // jpg alias for jpeg
        assert_eq!(ImageFormat::from_mime("image/jpg"), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::from_mime("image/tiff"), None);
    }

    #[test]
    fn image_format_from_extension_is_case_insensitive() {
        assert_eq!(ImageFormat::from_extension("png"), Some(ImageFormat::Png));
        assert_eq!(ImageFormat::from_extension("PNG"), Some(ImageFormat::Png));
        assert_eq!(ImageFormat::from_extension("jpg"), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::from_extension("jpeg"), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::from_extension("webp"), Some(ImageFormat::Webp));
        assert_eq!(ImageFormat::from_extension("tiff"), None);
    }
}

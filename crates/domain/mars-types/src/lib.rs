//! core value types shared across MARS. pure data, no i/o, no async.

#![forbid(unsafe_code)]

mod bbox;
mod binding;
mod content;
mod ids;
mod manifest;
mod spatial;

pub use bbox::Bbox;
pub use binding::{BindingMetadata, LevelMetadata};
pub use content::{ArtifactEntry, ContentHash, ImageFormat};
pub use ids::{
    ArtifactKey, ArtifactKeyError, BindingId, BindingIdError, CrsCode, LayerId, RequestId, SourceCollectionId,
};
pub use manifest::{Manifest, RasterLayerEntry};
pub use spatial::{DecimationLevel, HilbertKey, LayerSidecarEntry, LayerSidecarKind, PageEntry, PageId, PageKey};

/// current `Manifest::format_version`. readers reject anything other than
/// this exact value - no floor, no "accept `<= max`" (see `mars-store-fs`
/// / `mars-store-s3` manifest readers). bump on any incompatible change to
/// the `Manifest` envelope.
pub const MANIFEST_FORMAT_VERSION: u32 = 1;

/// upper bound on the on-disk pointer string. Versions are `v\d+` so 32 chars
/// (`v` + 31 decimal digits) covers anything `u64` can represent comfortably.
const MANIFEST_POINTER_MAX_LEN: usize = 32;

/// Validate a manifest pointer (`vN`, N a positive integer, capped length).
/// Both manifest-store adapters consume this so the contract for a "valid
/// pointer" lives in one place; lax acceptance (dotted names, very long
/// strings) is a footgun for the GC / rollover path.
pub fn validate_manifest_pointer(pointer: &str) -> Result<(), ManifestPointerError> {
    if pointer.is_empty() {
        return Err(ManifestPointerError::Empty);
    }
    if pointer.len() > MANIFEST_POINTER_MAX_LEN {
        return Err(ManifestPointerError::TooLong);
    }
    let Some(rest) = pointer.strip_prefix('v') else {
        return Err(ManifestPointerError::BadShape);
    };
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ManifestPointerError::BadShape);
    }
    Ok(())
}

/// Reasons a manifest pointer string fails validation.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestPointerError {
    #[error("manifest pointer is empty")]
    Empty,
    #[error("manifest pointer exceeds {} chars", MANIFEST_POINTER_MAX_LEN)]
    TooLong,
    #[error(r#"manifest pointer must match `v\d+`"#)]
    BadShape,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_pointer_accepts_versioned() {
        assert!(validate_manifest_pointer("v1").is_ok());
        assert!(validate_manifest_pointer("v0").is_ok());
        assert!(validate_manifest_pointer("v9999999").is_ok());
    }

    #[test]
    fn manifest_pointer_rejects_garbage() {
        assert_eq!(validate_manifest_pointer(""), Err(ManifestPointerError::Empty));
        assert_eq!(validate_manifest_pointer("v"), Err(ManifestPointerError::BadShape));
        assert_eq!(validate_manifest_pointer("vAA"), Err(ManifestPointerError::BadShape));
        assert_eq!(validate_manifest_pointer("1"), Err(ManifestPointerError::BadShape));
        assert_eq!(validate_manifest_pointer("v1.0"), Err(ManifestPointerError::BadShape));
        assert_eq!(
            validate_manifest_pointer("../etc/passwd"),
            Err(ManifestPointerError::BadShape)
        );
        let big = format!("v{}", "1".repeat(MANIFEST_POINTER_MAX_LEN));
        assert_eq!(validate_manifest_pointer(&big), Err(ManifestPointerError::TooLong));
    }
}

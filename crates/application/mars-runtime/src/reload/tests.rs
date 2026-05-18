use mars_observability::reject_reason;
use mars_store::StoreError;
use mars_types::ArtifactKey;

use super::classify_store_error;

#[test]
fn unsupported_format_version_maps_to_unsupported_label() {
    let err = StoreError::UnsupportedManifestVersion {
        found: 99,
        supported: 1,
    };
    assert_eq!(classify_store_error(&err), reject_reason::UNSUPPORTED_FORMAT_VERSION);
}

#[test]
fn hash_mismatch_maps_to_hash_mismatch_label() {
    let err = StoreError::HashMismatch {
        key: ArtifactKey::new("k"),
    };
    assert_eq!(classify_store_error(&err), reject_reason::HASH_MISMATCH);
}

#[test]
fn backend_error_falls_through_to_io_error() {
    let err = StoreError::Backend("boom".into());
    assert_eq!(classify_store_error(&err), reject_reason::IO_ERROR);
}

#[test]
fn transient_error_falls_through_to_io_error() {
    let err = StoreError::Transient("blip".into());
    assert_eq!(classify_store_error(&err), reject_reason::IO_ERROR);
}

#[test]
fn not_found_falls_through_to_io_error() {
    let err = StoreError::NotFound(ArtifactKey::new("k"));
    assert_eq!(classify_store_error(&err), reject_reason::IO_ERROR);
}

#[test]
fn not_implemented_falls_through_to_io_error() {
    let err = StoreError::NotImplemented { what: "x" };
    assert_eq!(classify_store_error(&err), reject_reason::IO_ERROR);
}

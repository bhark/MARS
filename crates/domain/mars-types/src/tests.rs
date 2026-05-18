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

//! key validation + path resolution for fs-backed stores.
//!
//! every store / cache method routes through `validate_key`. this is the only
//! place that turns an `ArtifactKey` (or raw `&str`) into an absolute path
//! under `root`, and it rejects every form of escape we know how to detect:
//! traversal, absolute, prefixed (windows), backslashes, NULs, empty segments,
//! and post-canonicalisation symlink escapes.

use std::path::{Component, Path, PathBuf};

use mars_store::StoreError;
use mars_types::ArtifactKey;

/// Validates `key` and returns the absolute path under `root` it refers to.
///
/// `root` must already be canonical and existing. The returned path is not
/// guaranteed to exist (callers writing new files need that). Symlink escapes
/// are detected by canonicalising the deepest existing ancestor and asserting
/// it lies under `root`.
pub(crate) fn validate_key(root: &Path, key: &str) -> Result<PathBuf, StoreError> {
    if key.is_empty() {
        return Err(StoreError::Backend("empty key".into()));
    }
    if key.contains('\\') {
        return Err(StoreError::Backend("backslash in key".into()));
    }
    if key.contains('\0') {
        return Err(StoreError::Backend("NUL byte in key".into()));
    }

    // syntactic split on '/' to detect empty segments, '.', '..', leading slash
    for seg in key.split('/') {
        match seg {
            "" => return Err(StoreError::Backend("empty path segment".into())),
            "." | ".." => return Err(StoreError::Backend("relative segment in key".into())),
            _ => {}
        }
    }

    let candidate = root.join(key);

    // structural check via Components - belt-and-braces
    for c in candidate.components() {
        match c {
            Component::ParentDir => {
                return Err(StoreError::Backend("parent-dir component".into()));
            }
            Component::Prefix(_) => {
                return Err(StoreError::Backend("prefix component".into()));
            }
            Component::Normal(name) => {
                if name.is_empty() {
                    return Err(StoreError::Backend("empty path component".into()));
                }
            }
            Component::RootDir | Component::CurDir => {}
        }
    }

    // resolve the deepest existing ancestor; if any of it is a symlink that
    // escapes root, reject. the file itself may not exist yet (put path).
    let anchor = deepest_existing(&candidate);
    let canon = anchor
        .canonicalize()
        .map_err(|e| StoreError::Backend(format!("canonicalise {}: {e}", anchor.display())))?;
    if !canon.starts_with(root) {
        return Err(StoreError::Backend("path escapes store root".into()));
    }

    // if the file itself exists, also re-canonicalise to catch a symlink at the
    // leaf that points outside (deepest_existing would have returned the leaf,
    // but only if it exists; canonicalize follows symlinks regardless).
    if candidate.exists() {
        let leaf_canon = candidate
            .canonicalize()
            .map_err(|e| StoreError::Backend(format!("canonicalise {}: {e}", candidate.display())))?;
        if !leaf_canon.starts_with(root) {
            return Err(StoreError::Backend("path escapes store root via symlink".into()));
        }
    }

    Ok(candidate)
}

/// Same as [`validate_key`] but takes an `ArtifactKey`, threading the key into
/// `NotFound` / `HashMismatch` errors at call sites.
pub(crate) fn validate_artifact_key(root: &Path, key: &ArtifactKey) -> Result<PathBuf, StoreError> {
    validate_key(root, key.as_str())
}

fn deepest_existing(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    loop {
        if p.exists() {
            return p;
        }
        match p.parent() {
            Some(parent) => p = parent.to_path_buf(),
            None => return p,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use tempfile::TempDir;

    fn root() -> (TempDir, PathBuf) {
        let td = TempDir::new().unwrap();
        let r = td.path().canonicalize().unwrap();
        (td, r)
    }

    #[test]
    fn accepts_normal_keys() {
        let (_td, root) = root();
        for k in ["a", "a/b/c.txt", "manifests/v1.json", "lyr/x/y/z.mars"] {
            assert!(validate_key(&root, k).is_ok(), "should accept {k}");
        }
    }

    #[test]
    fn rejects_invalid_keys() {
        let (_td, root) = root();
        for k in [
            "", "/abs", "..", "a/../b", "a//b", "a\\b", "a\0b", "../b", "./a", "a/./b",
        ] {
            assert!(validate_key(&root, k).is_err(), "should reject {k:?}");
        }
    }

    fn good_seg() -> impl Strategy<Value = String> {
        // single normal segment: alnum / dash / underscore, length 1..8
        "[a-z0-9_-][a-z0-9._-]{0,7}".prop_filter("not . or ..", |s: &String| s != "." && s != "..")
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1024))]

        #[test]
        fn proptest_classifies(
            kind in 0u8..6,
            seg in good_seg(),
            seg2 in good_seg(),
        ) {
            let td = TempDir::new().unwrap();
            let root = td.path().canonicalize().unwrap();
            let (key, expect_ok) = match kind {
                0 => (seg.clone(), true),
                1 => (format!("{seg}/{seg2}"), true),
                2 => (format!("{seg}/../{seg2}"), false),
                3 => (format!("/{seg}"), false),
                4 => (format!("{seg}//{seg2}"), false),
                _ => (format!("{seg}\\{seg2}"), false),
            };
            let r = validate_key(&root, &key);
            prop_assert_eq!(r.is_ok(), expect_ok, "key={} got={:?}", key, r);
        }
    }
}

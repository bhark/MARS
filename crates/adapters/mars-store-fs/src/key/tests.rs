#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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

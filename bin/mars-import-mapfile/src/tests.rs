#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn parse_bands_arg_validates_strict_increase() {
    assert!(parse_bands_arg("a:100,b:50").is_err());
    let ok = parse_bands_arg("a:100,b:200,c:max").unwrap();
    assert_eq!(
        ok,
        vec![("a".into(), 100u64), ("b".into(), 200u64), ("c".into(), u64::MAX),]
    );
}

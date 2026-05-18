#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;

use crate::{Change, DefinitionBytes, DefinitionSourceError};

#[test]
fn definition_bytes_round_trip() {
    let payload = b"layers: []\n";
    let d = DefinitionBytes {
        data: Bytes::from_static(payload),
        revision: "abc123".into(),
    };
    assert_eq!(d.data.as_ref(), payload);
    assert_eq!(d.revision, "abc123");
}

#[test]
fn change_equality() {
    let a = Change { revision: "v1".into() };
    let b = Change { revision: "v1".into() };
    let c = Change { revision: "v2".into() };
    assert_eq!(a, b);
    assert_ne!(a, c);
}

#[test]
fn error_display_covers_each_variant() {
    let e = DefinitionSourceError::NotImplemented { what: "watch" };
    assert_eq!(e.to_string(), "not implemented: watch");

    let e = DefinitionSourceError::NotFound {
        what: "configmap/foo".into(),
    };
    assert_eq!(e.to_string(), "not found: configmap/foo");

    let e = DefinitionSourceError::Auth {
        what: "git basic creds rejected".into(),
    };
    assert_eq!(e.to_string(), "auth error: git basic creds rejected");

    let e = DefinitionSourceError::Other { message: "tilt".into() };
    assert_eq!(e.to_string(), "other: tilt");
}

#[test]
fn network_helper_preserves_source_chain() {
    let inner = std::io::Error::other("dns down");
    let e = DefinitionSourceError::network("git fetch", inner);
    assert_eq!(e.to_string(), "network error: git fetch");
    let src = std::error::Error::source(&e).expect("source preserved");
    assert!(src.to_string().contains("dns down"));
}

#[test]
fn decode_helper_preserves_source_chain() {
    let bad: Vec<u8> = vec![0xff, 0xfe];
    let inner = std::str::from_utf8(&bad).unwrap_err();
    let e = DefinitionSourceError::decode("configmap value", inner);
    assert_eq!(e.to_string(), "decode error: configmap value");
    assert!(std::error::Error::source(&e).is_some());
}

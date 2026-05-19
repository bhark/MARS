use std::time::Duration;

use crate::{S3ConfigError, S3Credentials, S3DefinitionSource, strip_etag_quotes};

const REGION: &str = "us-east-1";
const BUCKET: &str = "mars-defs";
const KEY: &str = "dagi/definition.yaml";

#[test]
fn rejects_empty_bucket() {
    let r = S3DefinitionSource::new(None, REGION.into(), "  ".into(), KEY.into(), None, None);
    assert!(matches!(r, Err(S3ConfigError::EmptyBucket)));
}

#[test]
fn rejects_empty_key() {
    let r = S3DefinitionSource::new(None, REGION.into(), BUCKET.into(), String::new(), None, None);
    assert!(matches!(r, Err(S3ConfigError::EmptyKey)));
}

#[test]
fn rejects_empty_region() {
    let r = S3DefinitionSource::new(None, String::new(), BUCKET.into(), KEY.into(), None, None);
    assert!(matches!(r, Err(S3ConfigError::EmptyRegion)));
}

#[test]
fn rejects_partial_credentials() {
    let r = S3DefinitionSource::new(
        None,
        REGION.into(),
        BUCKET.into(),
        KEY.into(),
        None,
        Some(S3Credentials {
            access_key: "AKIA".into(),
            secret_key: String::new(),
            session_token: None,
        }),
    );
    assert!(matches!(r, Err(S3ConfigError::IncompleteCredentials)));
}

#[test]
fn default_cred_chain_when_no_secret() {
    // omitting `credentials` => default cred chain; constructor must succeed
    // without any AWS env vars set (resolution is deferred to the first I/O).
    let r = S3DefinitionSource::new(None, REGION.into(), BUCKET.into(), KEY.into(), None, None);
    assert!(r.is_ok());
}

#[test]
fn explicit_credentials_with_session_token() {
    let r = S3DefinitionSource::new(
        None,
        REGION.into(),
        BUCKET.into(),
        KEY.into(),
        Some(Duration::from_secs(30)),
        Some(S3Credentials {
            access_key: "AKIA".into(),
            secret_key: "secret".into(),
            session_token: Some("sts-token".into()),
        }),
    );
    assert!(r.is_ok());
}

#[test]
fn custom_endpoint_accepted() {
    let r = S3DefinitionSource::new(
        Some("http://minio.local:9000".into()),
        REGION.into(),
        BUCKET.into(),
        KEY.into(),
        None,
        Some(S3Credentials {
            access_key: "minio".into(),
            secret_key: "minio-secret".into(),
            session_token: None,
        }),
    );
    assert!(r.is_ok());
}

#[test]
fn etag_quote_stripping() {
    assert_eq!(strip_etag_quotes("\"deadbeef\"".into()), "deadbeef");
    assert_eq!(strip_etag_quotes("deadbeef".into()), "deadbeef");
    assert_eq!(strip_etag_quotes("\"\"".into()), "");
    // weak validator: W/"abc". the leading W/ is part of the value; the
    // surrounding quotes are inside, so strip only when both ends are quotes.
    assert_eq!(strip_etag_quotes("W/\"abc\"".into()), "W/\"abc\"");
}

#[test]
fn credentials_debug_redacts() {
    let c = S3Credentials {
        access_key: "AKIAIOSFODNN7EXAMPLE".into(),
        secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
        session_token: Some("FQoGZXIvYXdz...".into()),
    };
    let dbg = format!("{c:?}");
    assert!(!dbg.contains("AKIAIOSFODNN7EXAMPLE"));
    assert!(!dbg.contains("wJalrXUtnFEMI"));
    assert!(!dbg.contains("FQoGZXIvYXdz"));
    assert!(dbg.contains("<redacted>"));
}

// live S3 / MinIO coverage is deferred to the operator's integration tier
// behind `#[cfg(feature = "integration")]`, mirroring mars-source-postgres
// and the git adapter.

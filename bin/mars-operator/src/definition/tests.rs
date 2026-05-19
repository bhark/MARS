#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use super::*;
use crate::crd::definition::{ConfigMapKeyRef, GitRef, GitRevision, S3Ref, SecretRef};

fn bytes(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

fn make_bundle(name: &str, entries: &[(&str, &str)]) -> ResolvedSecret {
    let mut data: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (k, v) in entries {
        data.insert((*k).into(), bytes(v));
    }
    ResolvedSecret {
        name: name.into(),
        data,
    }
}

fn spec_inline(payload: &str) -> DefinitionSpec {
    DefinitionSpec {
        inline: Some(payload.into()),
        ..Default::default()
    }
}

fn spec_config_map(name: &str, key: &str) -> DefinitionSpec {
    DefinitionSpec {
        config_map_ref: Some(ConfigMapKeyRef {
            name: name.into(),
            key: key.into(),
        }),
        ..Default::default()
    }
}

fn spec_git_https(secret_ref: Option<&str>) -> DefinitionSpec {
    DefinitionSpec {
        git_ref: Some(GitRef {
            url: "https://example.com/repo.git".into(),
            git_ref: GitRevision {
                branch: Some("main".into()),
                ..Default::default()
            },
            path: "def.yaml".into(),
            interval: None,
            secret_ref: secret_ref.map(|n| SecretRef { name: n.into() }),
        }),
        ..Default::default()
    }
}

fn spec_git_ssh(secret_ref: Option<&str>) -> DefinitionSpec {
    DefinitionSpec {
        git_ref: Some(GitRef {
            url: "git@example.com:org/repo.git".into(),
            git_ref: GitRevision {
                branch: Some("main".into()),
                ..Default::default()
            },
            path: "def.yaml".into(),
            interval: None,
            secret_ref: secret_ref.map(|n| SecretRef { name: n.into() }),
        }),
        ..Default::default()
    }
}

fn spec_s3(secret_ref: Option<&str>) -> DefinitionSpec {
    DefinitionSpec {
        s3_ref: Some(S3Ref {
            endpoint: None,
            region: "us-east-1".into(),
            bucket: "defs".into(),
            key: "def.yaml".into(),
            interval: None,
            secret_ref: secret_ref.map(|n| SecretRef { name: n.into() }),
        }),
        ..Default::default()
    }
}

// Box<dyn DefinitionSource> has no Debug, so .expect_err/.unwrap don't compile.
// helpers narrow the result before assertion.
fn err_of(res: Result<Box<dyn DefinitionSource>, ResolveError>) -> ResolveError {
    match res {
        Ok(_) => panic!("expected Err, got Ok"),
        Err(e) => e,
    }
}

fn ok_of(res: Result<Box<dyn DefinitionSource>, ResolveError>) {
    if let Err(e) = res {
        panic!("expected Ok, got Err: {e:?}");
    }
}

// ---- variant detection ------------------------------------------------------

#[test]
fn zero_variants_rejected() {
    let spec = DefinitionSpec::default();
    let err = err_of(resolve_from_resolved(&spec, None));
    assert!(matches!(err, ResolveError::ExactlyOneVariant { got: 0 }));
}

#[test]
fn two_variants_rejected() {
    let mut spec = spec_inline("hello");
    spec.config_map_ref = Some(ConfigMapKeyRef {
        name: "x".into(),
        key: "y".into(),
    });
    let err = err_of(resolve_from_resolved(&spec, None));
    assert!(matches!(err, ResolveError::ExactlyOneVariant { got: 2 }));
}

#[test]
fn inline_dispatches() {
    let spec = spec_inline("service: {}\n");
    ok_of(resolve_from_resolved(&spec, None));
}

#[test]
fn configmap_unreachable_via_pure_dispatch() {
    // configmap path is owned by resolve(), which has the kube client + ns.
    // pure dispatch falls through to the safety-belt error.
    let spec = spec_config_map("dagi-def", "definition.yaml");
    let err = err_of(resolve_from_resolved(&spec, None));
    assert!(matches!(err, ResolveError::ExactlyOneVariant { .. }));
}

// ---- git auth disambiguation -----------------------------------------------

#[test]
fn git_no_secret_is_public_no_tls() {
    let spec = spec_git_https(None);
    ok_of(resolve_from_resolved(&spec, None));
}

#[test]
fn git_ssh_bundle_selects_ssh() {
    let bundle = make_bundle(
        "git-ssh",
        &[
            ("identity", "PRIVATE"),
            ("identity.pub", "PUBLIC"),
            ("known_hosts", "example.com ssh-ed25519 AAAA"),
        ],
    );
    let (auth, tls) = git_auth_from_bundle(Some(&bundle), true).expect("ssh ok");
    assert!(matches!(auth, GitAuth::SshKey { .. }));
    assert!(tls.is_none());
}

#[test]
fn git_basic_bundle_selects_basic() {
    let bundle = make_bundle("git-basic", &[("username", "alice"), ("password", "hunter2")]);
    let (auth, _) = git_auth_from_bundle(Some(&bundle), true).expect("basic ok");
    assert!(matches!(auth, GitAuth::BasicAuth { .. }));
}

#[test]
fn git_bearer_bundle_selects_bearer() {
    let bundle = make_bundle("git-bearer", &[("bearerToken", "ghp_xxx")]);
    let (auth, _) = git_auth_from_bundle(Some(&bundle), true).expect("bearer ok");
    assert!(matches!(auth, GitAuth::BearerToken(_)));
}

#[test]
fn git_mixed_modes_rejected() {
    let bundle = make_bundle(
        "mixed",
        &[
            ("identity", "k"),
            ("identity.pub", "p"),
            ("known_hosts", "h"),
            ("username", "u"),
            ("password", "p"),
        ],
    );
    let err = git_auth_from_bundle(Some(&bundle), true).expect_err("must reject");
    assert!(matches!(err, ResolveError::ConflictingGitAuthModes { .. }));
}

#[test]
fn git_ca_only_yields_public_plus_tls_ca() {
    let bundle = make_bundle("ca-only", &[("ca.crt", "-----BEGIN CERTIFICATE-----")]);
    let (auth, tls) = git_auth_from_bundle(Some(&bundle), true).expect("ca-only ok");
    assert!(matches!(auth, GitAuth::None));
    let tls = tls.expect("tls bundle");
    assert!(tls.ca_cert.is_some());
    assert!(tls.client_cert.is_none());
    assert!(tls.client_key.is_none());
}

#[test]
fn git_mtls_with_basic_auth_layers_correctly() {
    let bundle = make_bundle(
        "mtls-basic",
        &[
            ("username", "alice"),
            ("password", "hunter2"),
            ("tls.crt", "CERT"),
            ("tls.key", "KEY"),
            ("ca.crt", "CA"),
        ],
    );
    let (auth, tls) = git_auth_from_bundle(Some(&bundle), true).expect("layered ok");
    assert!(matches!(auth, GitAuth::BasicAuth { .. }));
    let tls = tls.expect("tls bundle");
    assert!(tls.client_cert.is_some() && tls.client_key.is_some() && tls.ca_cert.is_some());
}

#[test]
fn git_ssh_missing_key_yields_field_missing() {
    let bundle = make_bundle("partial-ssh", &[("identity", "k"), ("identity.pub", "p")]);
    let err = git_auth_from_bundle(Some(&bundle), true).expect_err("must require known_hosts");
    assert!(matches!(err, ResolveError::SecretFieldMissing { field, .. } if field == "known_hosts"));
}

#[test]
fn git_basic_missing_password_yields_field_missing() {
    let bundle = make_bundle("partial-basic", &[("username", "alice")]);
    let err = git_auth_from_bundle(Some(&bundle), true).expect_err("must require password");
    assert!(matches!(err, ResolveError::SecretFieldMissing { field, .. } if field == "password"));
}

// ---- git revision selection -------------------------------------------------

#[test]
fn git_revision_zero_set_rejected() {
    let rev = GitRevision::default();
    assert!(matches!(git_reference(&rev), Err(ResolveError::ExactlyOneGitRevision)));
}

#[test]
fn git_revision_two_set_rejected() {
    let rev = GitRevision {
        branch: Some("main".into()),
        tag: Some("v1".into()),
        commit: None,
    };
    assert!(matches!(git_reference(&rev), Err(ResolveError::ExactlyOneGitRevision)));
}

#[test]
fn git_revision_branch_resolves() {
    let rev = GitRevision {
        branch: Some("main".into()),
        ..Default::default()
    };
    assert_eq!(git_reference(&rev).unwrap(), GitReference::Branch("main".into()));
}

#[test]
fn git_revision_tag_resolves() {
    let rev = GitRevision {
        tag: Some("v1.2.3".into()),
        ..Default::default()
    };
    assert_eq!(git_reference(&rev).unwrap(), GitReference::Tag("v1.2.3".into()));
}

#[test]
fn git_revision_commit_resolves() {
    let rev = GitRevision {
        commit: Some("deadbeef".into()),
        ..Default::default()
    };
    assert_eq!(git_reference(&rev).unwrap(), GitReference::Commit("deadbeef".into()));
}

// ---- s3 credential handling -------------------------------------------------

#[test]
fn s3_no_secret_yields_none_credentials() {
    assert!(s3_credentials_from_bundle(None, false).expect("none ok").is_none());
}

#[test]
fn s3_full_credentials() {
    let bundle = make_bundle("s3-creds", &[("accessKey", "AKIA"), ("secretKey", "SECRET")]);
    let creds = s3_credentials_from_bundle(Some(&bundle), true)
        .expect("full ok")
        .expect("creds present");
    assert_eq!(creds.access_key, "AKIA");
    assert_eq!(creds.secret_key, "SECRET");
    assert!(creds.session_token.is_none());
}

#[test]
fn s3_credentials_with_session_token() {
    let bundle = make_bundle(
        "s3-sts",
        &[
            ("accessKey", "AKIA"),
            ("secretKey", "SECRET"),
            ("sessionToken", "TOKEN"),
        ],
    );
    let creds = s3_credentials_from_bundle(Some(&bundle), true)
        .expect("ok")
        .expect("creds");
    assert_eq!(creds.session_token.as_deref(), Some("TOKEN"));
}

#[test]
fn s3_partial_credentials_rejected() {
    let bundle = make_bundle("partial-s3", &[("accessKey", "AKIA")]);
    let err = s3_credentials_from_bundle(Some(&bundle), true).expect_err("must reject");
    assert!(matches!(err, ResolveError::IncompleteS3Credentials { .. }));
}

#[test]
fn s3_empty_bundle_rejected() {
    let bundle = make_bundle("empty-s3", &[]);
    let err = s3_credentials_from_bundle(Some(&bundle), true).expect_err("must reject");
    assert!(matches!(err, ResolveError::IncompleteS3Credentials { .. }));
}

// ---- pure-dispatch end-to-end for credentialed variants --------------------

#[test]
fn git_dispatch_public_https() {
    let spec = spec_git_https(None);
    ok_of(resolve_from_resolved(&spec, None));
}

#[test]
fn git_dispatch_ssh_bundle() {
    let spec = spec_git_ssh(Some("git-ssh"));
    let bundle = make_bundle(
        "git-ssh",
        &[
            ("identity", "PRIVATE"),
            ("identity.pub", "PUBLIC"),
            ("known_hosts", "example.com ssh-ed25519 AAAA"),
        ],
    );
    ok_of(resolve_from_resolved(&spec, Some(bundle)));
}

#[test]
fn s3_dispatch_with_credentials() {
    let spec = spec_s3(Some("s3-creds"));
    let bundle = make_bundle("s3-creds", &[("accessKey", "AKIA"), ("secretKey", "SECRET")]);
    ok_of(resolve_from_resolved(&spec, Some(bundle)));
}

#[test]
fn s3_dispatch_default_chain_when_no_secret() {
    let spec = spec_s3(None);
    ok_of(resolve_from_resolved(&spec, None));
}

// ---- interval parsing -------------------------------------------------------

#[test]
fn interval_none_passes_through() {
    assert!(parse_optional_interval(None).unwrap().is_none());
}

#[test]
fn interval_humantime_parses() {
    let d = parse_optional_interval(Some("30s")).unwrap().unwrap();
    assert_eq!(d, std::time::Duration::from_secs(30));
}

#[test]
fn interval_invalid_rejected() {
    let err = parse_optional_interval(Some("not-a-duration")).expect_err("must reject");
    assert!(matches!(err, ResolveError::InvalidInterval { .. }));
}

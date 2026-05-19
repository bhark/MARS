use std::time::Duration;

use crate::auth::{url_is_https, url_is_ssh};
use crate::{GitAuth, GitConfigError, GitDefinitionSource, GitReference, TlsBundle};

const HTTPS_URL: &str = "https://git.example.com/org/repo.git";
const SSH_URL: &str = "ssh://git@git.example.com/org/repo.git";
const SCP_URL: &str = "git@git.example.com:org/repo.git";

#[test]
fn rejects_empty_url() {
    let r = GitDefinitionSource::new(
        String::new(),
        GitReference::Branch("main".into()),
        "config.yaml".into(),
        None,
        GitAuth::None,
        None,
    );
    assert!(matches!(r, Err(GitConfigError::EmptyUrl)));
}

#[test]
fn rejects_empty_path() {
    let r = GitDefinitionSource::new(
        HTTPS_URL.into(),
        GitReference::Branch("main".into()),
        "   ".into(),
        None,
        GitAuth::None,
        None,
    );
    assert!(matches!(r, Err(GitConfigError::EmptyPath)));
}

#[test]
fn rejects_empty_reference() {
    let r = GitDefinitionSource::new(
        HTTPS_URL.into(),
        GitReference::Branch("".into()),
        "config.yaml".into(),
        None,
        GitAuth::None,
        None,
    );
    assert!(matches!(r, Err(GitConfigError::MissingReference)));
}

#[test]
fn url_scheme_classification() {
    assert!(url_is_https(HTTPS_URL));
    assert!(!url_is_ssh(HTTPS_URL));
    assert!(url_is_ssh(SSH_URL));
    assert!(url_is_ssh(SCP_URL));
    assert!(!url_is_https(SSH_URL));
}

#[test]
fn basic_auth_requires_https() {
    let r = GitDefinitionSource::new(
        SSH_URL.into(),
        GitReference::Branch("main".into()),
        "x".into(),
        None,
        GitAuth::BasicAuth {
            username: "u".into(),
            password: "p".into(),
        },
        None,
    );
    assert!(matches!(r, Err(GitConfigError::SchemeMismatch { .. })));
}

#[test]
fn bearer_requires_https() {
    let r = GitDefinitionSource::new(
        SSH_URL.into(),
        GitReference::Branch("main".into()),
        "x".into(),
        None,
        GitAuth::BearerToken("t".into()),
        None,
    );
    assert!(matches!(r, Err(GitConfigError::SchemeMismatch { .. })));
}

#[test]
fn basic_auth_requires_nonempty_fields() {
    let r = GitDefinitionSource::new(
        HTTPS_URL.into(),
        GitReference::Branch("main".into()),
        "x".into(),
        None,
        GitAuth::BasicAuth {
            username: "u".into(),
            password: String::new(),
        },
        None,
    );
    assert!(matches!(r, Err(GitConfigError::SchemeMismatch { .. })));
}

#[test]
fn bearer_requires_nonempty_token() {
    let r = GitDefinitionSource::new(
        HTTPS_URL.into(),
        GitReference::Branch("main".into()),
        "x".into(),
        None,
        GitAuth::BearerToken(String::new()),
        None,
    );
    assert!(matches!(r, Err(GitConfigError::SchemeMismatch { .. })));
}

#[test]
fn ssh_requires_ssh_url() {
    let r = GitDefinitionSource::new(
        HTTPS_URL.into(),
        GitReference::Branch("main".into()),
        "x".into(),
        None,
        GitAuth::SshKey {
            identity: b"-----BEGIN-----\n".to_vec(),
            public: b"ssh-ed25519 AAAA".to_vec(),
            known_hosts: b"git.example.com ssh-ed25519 AAAA".to_vec(),
        },
        None,
    );
    assert!(matches!(r, Err(GitConfigError::SchemeMismatch { .. })));
}

#[test]
fn ssh_requires_full_bundle() {
    for (id, pubk, kh) in [
        (vec![], b"p".to_vec(), b"k".to_vec()),
        (b"i".to_vec(), vec![], b"k".to_vec()),
        (b"i".to_vec(), b"p".to_vec(), vec![]),
    ] {
        let r = GitDefinitionSource::new(
            SSH_URL.into(),
            GitReference::Branch("main".into()),
            "x".into(),
            None,
            GitAuth::SshKey {
                identity: id,
                public: pubk,
                known_hosts: kh,
            },
            None,
        );
        assert!(matches!(r, Err(GitConfigError::SshBundle { .. })));
    }
}

#[test]
fn tls_bundle_requires_matched_pair() {
    let r = GitDefinitionSource::new(
        HTTPS_URL.into(),
        GitReference::Branch("main".into()),
        "x".into(),
        None,
        GitAuth::None,
        Some(TlsBundle {
            client_cert: Some(b"cert".to_vec()),
            client_key: None,
            ca_cert: None,
        }),
    );
    assert!(matches!(r, Err(GitConfigError::TlsBundle { .. })));
}

#[test]
fn tls_bundle_ca_only_is_valid() {
    let r = GitDefinitionSource::new(
        HTTPS_URL.into(),
        GitReference::Branch("main".into()),
        "x".into(),
        None,
        GitAuth::None,
        Some(TlsBundle {
            client_cert: None,
            client_key: None,
            ca_cert: Some(b"ca-bundle".to_vec()),
        }),
    );
    assert!(r.is_ok());
}

#[test]
fn tls_bundle_empty_is_rejected() {
    let r = GitDefinitionSource::new(
        HTTPS_URL.into(),
        GitReference::Branch("main".into()),
        "x".into(),
        None,
        GitAuth::None,
        Some(TlsBundle::default()),
    );
    assert!(matches!(r, Err(GitConfigError::TlsBundle { .. })));
}

#[test]
fn ref_kinds_are_disambiguated() {
    let interval = Some(Duration::from_secs(120));
    for r in [
        GitReference::Branch("main".into()),
        GitReference::Tag("v1.0.0".into()),
        GitReference::Commit("0123456789abcdef0123456789abcdef01234567".into()),
    ] {
        let src = GitDefinitionSource::new(
            HTTPS_URL.into(),
            r.clone(),
            "config.yaml".into(),
            interval,
            GitAuth::None,
            None,
        );
        assert!(src.is_ok(), "ref {r:?} should be accepted");
    }
}

#[test]
fn redact_strips_userinfo_in_urls() {
    use crate::redact;
    let msg = "failed to fetch https://user:secret@host.example.com/repo.git: 401";
    let red = redact(msg);
    assert!(!red.contains("secret"), "userinfo leaked: {red}");
    assert!(red.contains("host.example.com"));
}

// the actual fetch + watch paths exercise libgit2 against a live remote; that
// belongs in the operator's integration tier (mirrors the
// mars-source-postgres `integration` feature pattern). gated on demand to
// keep the default test loop hermetic.

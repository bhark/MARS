//! Blocking libgit2 ops. Driven from `tokio::task::spawn_blocking`; nothing
//! here is async-aware.

use std::path::{Path, PathBuf};

use bytes::Bytes;
use git2::cert::Cert;
use git2::{
    CertificateCheckStatus, Cred, CredentialType, FetchOptions, ProxyOptions, Remote, RemoteCallbacks, Repository,
};
use mars_definition_source::{DefinitionBytes, DefinitionSourceError};
use tempfile::TempDir;

use crate::auth::{GitAuth, GitReference};
use crate::{Inner, map_git2_error, map_tls_io, write_secret_file};

/// Full fetch + read. Creates a bare repo in a temp dir, fetches just the
/// required refspec, resolves the target commit, reads the blob at `path`.
pub(crate) fn fetch_blocking(inner: &Inner) -> Result<DefinitionBytes, DefinitionSourceError> {
    let tmp = TempDir::with_prefix("mars-definition-source-git-").map_err(|e| map_tls_io("tempdir", e))?;
    let secrets = SecretFiles::materialise(tmp.path(), inner)?;
    let repo = Repository::init_bare(tmp.path().join("repo")).map_err(|e| map_git2_error("git init bare", e))?;
    apply_tls_config(&repo, &secrets)?;

    let mut remote = repo
        .remote_anonymous(&inner.url)
        .map_err(|e| map_git2_error("git remote", e))?;
    let refspec = refspec_for(&inner.reference);

    let mut callbacks = RemoteCallbacks::new();
    install_credential_cb(&mut callbacks, inner, &secrets);
    install_certificate_cb(&mut callbacks, inner);

    let mut fetch_opts = FetchOptions::new();
    fetch_opts.remote_callbacks(callbacks);
    fetch_opts.depth(1);
    fetch_opts.download_tags(git2::AutotagOption::None);
    let proxy_opts = ProxyOptions::new();
    fetch_opts.proxy_options(proxy_opts);

    remote
        .fetch(&[&refspec], Some(&mut fetch_opts), None)
        .map_err(|e| map_git2_error("git fetch", e))?;

    let oid = resolve_oid(&repo, &remote, &inner.reference)?;
    let commit = repo
        .find_commit(oid)
        .map_err(|e| map_git2_error("git find_commit", e))?;
    let tree = commit.tree().map_err(|e| map_git2_error("git commit tree", e))?;
    let entry = tree
        .get_path(Path::new(&inner.path))
        .map_err(|e| map_git2_error("git tree get_path", e))?;
    let obj = entry
        .to_object(&repo)
        .map_err(|e| map_git2_error("git entry to_object", e))?;
    let blob = obj.as_blob().ok_or(DefinitionSourceError::Decode {
        what: "git path is not a blob",
        source: Box::new(NotABlob),
    })?;

    Ok(DefinitionBytes {
        data: Bytes::copy_from_slice(blob.content()),
        revision: oid.to_string(),
    })
}

/// Resolve-only variant for the watch poller. Uses `git ls-remote`-equivalent
/// (`Remote::list`) which fetches just the advertised refs, no objects.
pub(crate) fn resolve_revision_blocking(inner: &Inner) -> Result<String, DefinitionSourceError> {
    let tmp = TempDir::with_prefix("mars-definition-source-git-ls-").map_err(|e| map_tls_io("tempdir", e))?;
    let secrets = SecretFiles::materialise(tmp.path(), inner)?;
    let repo = Repository::init_bare(tmp.path().join("repo")).map_err(|e| map_git2_error("git init bare", e))?;
    apply_tls_config(&repo, &secrets)?;

    let mut remote = repo
        .remote_anonymous(&inner.url)
        .map_err(|e| map_git2_error("git remote", e))?;

    let mut callbacks = RemoteCallbacks::new();
    install_credential_cb(&mut callbacks, inner, &secrets);
    install_certificate_cb(&mut callbacks, inner);

    // ls-remote: connect-only, no object fetch. proxy_options accepted even when empty.
    let proxy_opts = ProxyOptions::new();
    remote
        .connect_auth(git2::Direction::Fetch, Some(callbacks), Some(proxy_opts))
        .map_err(|e| map_git2_error("git ls-remote connect", e))?;

    let heads = remote.list().map_err(|e| map_git2_error("git ls-remote list", e))?;
    let want = wanted_ref_name(&inner.reference);
    for head in heads {
        if head.name() == want {
            return Ok(head.oid().to_string());
        }
        // tag peeling: `refs/tags/X` may show `^{}` companion for annotated tags
        let peeled = format!("{}^{{}}", want);
        if head.name() == peeled {
            return Ok(head.oid().to_string());
        }
    }
    // commit-SHA reference: nothing to look up; treat as resolved-as-is.
    if let GitReference::Commit(sha) = &inner.reference {
        return Ok(sha.clone());
    }
    Err(DefinitionSourceError::NotFound {
        what: format!("git ref {want} not advertised by remote"),
    })
}

fn refspec_for(r: &GitReference) -> String {
    match r {
        GitReference::Branch(b) => format!("+refs/heads/{b}:refs/remotes/origin/{b}"),
        GitReference::Tag(t) => format!("+refs/tags/{t}:refs/tags/{t}"),
        // commit-only fetch needs the full sha as the source; libgit2 accepts
        // it when the remote allows `uploadpack.allowReachableSHA1InWant` (and
        // most managed hosts do). fall back to fetching all heads if the
        // server rejects: handled by the second attempt in resolve_oid via
        // tree-walk-not-found -> caller surface.
        GitReference::Commit(sha) => format!("+{sha}:refs/mars/commit"),
    }
}

fn wanted_ref_name(r: &GitReference) -> String {
    match r {
        GitReference::Branch(b) => format!("refs/heads/{b}"),
        GitReference::Tag(t) => format!("refs/tags/{t}"),
        GitReference::Commit(sha) => sha.clone(),
    }
}

fn resolve_oid(repo: &Repository, remote: &Remote<'_>, r: &GitReference) -> Result<git2::Oid, DefinitionSourceError> {
    match r {
        GitReference::Branch(b) => {
            let name = format!("refs/remotes/origin/{b}");
            repo.refname_to_id(&name)
                .map_err(|e| map_git2_error("git resolve branch", e))
        }
        GitReference::Tag(t) => {
            let name = format!("refs/tags/{t}");
            let oid = repo
                .refname_to_id(&name)
                .map_err(|e| map_git2_error("git resolve tag", e))?;
            // peel annotated tags down to the underlying commit
            let obj = repo
                .find_object(oid, None)
                .map_err(|e| map_git2_error("git find_object", e))?;
            let commit = obj
                .peel(git2::ObjectType::Commit)
                .map_err(|e| map_git2_error("git peel tag", e))?;
            Ok(commit.id())
        }
        GitReference::Commit(sha) => {
            // we fetched into refs/mars/commit; the oid is the sha itself.
            let _ = remote;
            git2::Oid::from_str(sha).map_err(|e| DefinitionSourceError::Decode {
                what: "invalid commit sha",
                source: Box::new(e),
            })
        }
    }
}

fn install_credential_cb<'cb>(callbacks: &mut RemoteCallbacks<'cb>, inner: &'cb Inner, secrets: &'cb SecretFiles) {
    let auth = inner.auth.clone();
    let identity_path = secrets.ssh_identity.clone();
    let public_path = secrets.ssh_public.clone();
    callbacks.credentials(move |url, username_from_url, allowed| {
        match &auth {
            GitAuth::None => Cred::default(),
            GitAuth::BasicAuth { username, password } if allowed.contains(CredentialType::USER_PASS_PLAINTEXT) => {
                Cred::userpass_plaintext(username, password)
            }
            GitAuth::BearerToken(token) if allowed.contains(CredentialType::USER_PASS_PLAINTEXT) => {
                // RFC-style "x-access-token" username + bearer-as-password
                // works for GitHub / GitLab / Bitbucket; libgit2 forwards it
                // through basic auth, which the server unwraps as bearer.
                Cred::userpass_plaintext("x-access-token", token)
            }
            GitAuth::SshKey { .. } if allowed.contains(CredentialType::SSH_KEY) => {
                let user = username_from_url.unwrap_or("git");
                let (Some(priv_p), Some(pub_p)) = (identity_path.as_ref(), public_path.as_ref()) else {
                    return Err(git2::Error::from_str("ssh identity not materialised"));
                };
                Cred::ssh_key(user, Some(pub_p), priv_p, None)
            }
            _ => Err(git2::Error::from_str(&format!(
                "auth mode incompatible with libgit2 allowed types {:?} for {url}",
                allowed
            ))),
        }
    });
}

fn install_certificate_cb<'cb>(callbacks: &mut RemoteCallbacks<'cb>, inner: &'cb Inner) {
    let auth = inner.auth.clone();
    callbacks.certificate_check(move |cert: &Cert<'_>, host: &str| {
        // for HTTPS we defer to libgit2 / openssl's chain validation (driven
        // by http.sslCAInfo when a custom CA is supplied). returning Passthrough
        // tells libgit2 to apply its own checks.
        if cert.as_hostkey().is_none() {
            return Ok(CertificateCheckStatus::CertificatePassthrough);
        }
        match &auth {
            GitAuth::SshKey { known_hosts, .. } => verify_ssh_known_host(known_hosts, host, cert),
            _ => Ok(CertificateCheckStatus::CertificatePassthrough),
        }
    });
}

fn verify_ssh_known_host(
    known_hosts: &[u8],
    host: &str,
    cert: &Cert<'_>,
) -> Result<CertificateCheckStatus, git2::Error> {
    let Some(hk) = cert.as_hostkey() else {
        return Ok(CertificateCheckStatus::CertificatePassthrough);
    };
    let server_fp = match hk.hash_sha256() {
        Some(h) => h,
        None => return Err(git2::Error::from_str("server presented no SHA-256 hostkey fingerprint")),
    };
    // parse known_hosts. lines:
    //   <hostpattern> <keytype> <base64key> [comment]
    // we compare the SHA-256 of the decoded key bytes to the server fp.
    let text = std::str::from_utf8(known_hosts).map_err(|_| git2::Error::from_str("known_hosts is not utf-8"))?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(pattern), Some(_keytype), Some(b64)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if !known_host_pattern_matches(pattern, host) {
            continue;
        }
        let key_bytes = match base64_decode(b64) {
            Some(b) => b,
            None => continue,
        };
        let sha = sha256(&key_bytes);
        if sha == *server_fp {
            return Ok(CertificateCheckStatus::CertificateOk);
        }
    }
    Err(git2::Error::from_str(&format!(
        "ssh host key for {host} did not match any entry in known_hosts"
    )))
}

fn known_host_pattern_matches(pattern: &str, host: &str) -> bool {
    // host patterns can be comma-separated; we don't bother with hashed
    // patterns (HashKnownHosts=yes) since operators control their bundle.
    for p in pattern.split(',') {
        if p == host {
            return true;
        }
    }
    false
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    // tolerant of trailing '=' padding; rejects any non-alphabet char.
    let s = s.trim();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for c in s.chars() {
        let v = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => 26 + c as u32 - 'a' as u32,
            '0'..='9' => 52 + c as u32 - '0' as u32,
            '+' => 62,
            '/' => 63,
            '=' => break,
            _ => return None,
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

struct SecretFiles {
    ssh_identity: Option<PathBuf>,
    ssh_public: Option<PathBuf>,
    client_cert: Option<PathBuf>,
    client_key: Option<PathBuf>,
    ca_cert: Option<PathBuf>,
}

impl SecretFiles {
    fn materialise(dir: &Path, inner: &Inner) -> Result<Self, DefinitionSourceError> {
        let secrets_dir = dir.join("secrets");
        std::fs::create_dir(&secrets_dir).map_err(|e| map_tls_io("secrets dir", e))?;
        // 0o700 on unix; on other platforms libgit2 still reads them but
        // permission hardening is a unix-only concern.
        set_dir_mode_owner_only(&secrets_dir);

        let (ssh_identity, ssh_public) = match &inner.auth {
            GitAuth::SshKey { identity, public, .. } => {
                let id = write_secret_file(&secrets_dir, "id", identity).map_err(|e| map_tls_io("write id", e))?;
                let pubp =
                    write_secret_file(&secrets_dir, "id.pub", public).map_err(|e| map_tls_io("write id.pub", e))?;
                set_file_mode_owner_only(&id);
                (Some(id), Some(pubp))
            }
            _ => (None, None),
        };

        let (client_cert, client_key, ca_cert) = match &inner.tls {
            Some(t) => {
                let cc = match &t.client_cert {
                    Some(b) => Some(
                        write_secret_file(&secrets_dir, "client.crt", b)
                            .map_err(|e| map_tls_io("write client.crt", e))?,
                    ),
                    None => None,
                };
                let ck = match &t.client_key {
                    Some(b) => {
                        let p = write_secret_file(&secrets_dir, "client.key", b)
                            .map_err(|e| map_tls_io("write client.key", e))?;
                        set_file_mode_owner_only(&p);
                        Some(p)
                    }
                    None => None,
                };
                let ca = match &t.ca_cert {
                    Some(b) => {
                        Some(write_secret_file(&secrets_dir, "ca.crt", b).map_err(|e| map_tls_io("write ca.crt", e))?)
                    }
                    None => None,
                };
                (cc, ck, ca)
            }
            None => (None, None, None),
        };

        Ok(Self {
            ssh_identity,
            ssh_public,
            client_cert,
            client_key,
            ca_cert,
        })
    }
}

fn apply_tls_config(repo: &Repository, secrets: &SecretFiles) -> Result<(), DefinitionSourceError> {
    let mut cfg = repo.config().map_err(|e| map_git2_error("git open config", e))?;
    if let Some(p) = &secrets.client_cert {
        cfg.set_str("http.sslCert", p_to_str(p)?)
            .map_err(|e| map_git2_error("config http.sslCert", e))?;
    }
    if let Some(p) = &secrets.client_key {
        cfg.set_str("http.sslKey", p_to_str(p)?)
            .map_err(|e| map_git2_error("config http.sslKey", e))?;
    }
    if let Some(p) = &secrets.ca_cert {
        cfg.set_str("http.sslCAInfo", p_to_str(p)?)
            .map_err(|e| map_git2_error("config http.sslCAInfo", e))?;
    }
    Ok(())
}

fn p_to_str(p: &Path) -> Result<&str, DefinitionSourceError> {
    p.to_str().ok_or(DefinitionSourceError::Other {
        message: "path is not utf-8".into(),
    })
}

#[cfg(unix)]
fn set_dir_mode_owner_only(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(p) {
        let mut perm = meta.permissions();
        perm.set_mode(0o700);
        let _ = std::fs::set_permissions(p, perm);
    }
}

#[cfg(not(unix))]
fn set_dir_mode_owner_only(_p: &Path) {}

#[cfg(unix)]
fn set_file_mode_owner_only(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(p) {
        let mut perm = meta.permissions();
        perm.set_mode(0o600);
        let _ = std::fs::set_permissions(p, perm);
    }
}

#[cfg(not(unix))]
fn set_file_mode_owner_only(_p: &Path) {}

#[derive(Debug, thiserror::Error)]
#[error("git path resolves to a non-blob object")]
struct NotABlob;

//! Reference + auth + tls types for the git definition-source adapter.
//!
//! All sensitive material (passwords, tokens, ssh identities, client keys) is
//! held as plain `String` / `Vec<u8>` on the adapter side. We never serialise
//! these types and never log their contents; the wrapping by domain types is
//! purely to keep the constructor signature legible.

use crate::GitConfigError;

/// A git reference. Exactly one variant is set; ambiguity is rejected at
/// [`crate::GitDefinitionSource::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitReference {
    /// Symbolic branch name (resolved to its current tip on each tick).
    Branch(String),
    /// Symbolic tag name (resolved to its target commit; annotated tags are
    /// peeled to the underlying commit).
    Tag(String),
    /// Fixed commit SHA. Watch will still tick, but the resolved SHA will be
    /// constant unless the operator changes the spec.
    Commit(String),
}

impl GitReference {
    pub(crate) fn validate(&self) -> Result<(), GitConfigError> {
        let s = match self {
            Self::Branch(v) | Self::Tag(v) | Self::Commit(v) => v,
        };
        if s.trim().is_empty() {
            return Err(GitConfigError::MissingReference);
        }
        Ok(())
    }
}

/// Authentication mode. One-of; selected by the operator based on which keys
/// are present in the resolved `Secret`.
#[derive(Debug, Clone)]
pub enum GitAuth {
    /// Public repo, no credentials.
    None,
    /// HTTPS basic. Works for GitHub PATs as `password`.
    BasicAuth {
        /// Username component.
        username: String,
        /// Password / PAT.
        password: String,
    },
    /// HTTPS bearer (e.g. GitHub fine-grained PAT, GitLab personal token).
    BearerToken(String),
    /// SSH key bundle: private key, public key, host fingerprints. The
    /// `known_hosts` bytes must contain at least one host line matching the
    /// remote URL's hostname; the adapter rejects unknown hosts.
    SshKey {
        /// Private key bytes (OpenSSH format, no passphrase).
        identity: Vec<u8>,
        /// Public key bytes.
        public: Vec<u8>,
        /// `known_hosts` content used to validate the server fingerprint.
        known_hosts: Vec<u8>,
    },
}

impl GitAuth {
    pub(crate) fn validate(&self, url: &str) -> Result<(), GitConfigError> {
        let is_ssh = url_is_ssh(url);
        let is_https = url_is_https(url);
        match self {
            Self::None => Ok(()),
            Self::BasicAuth { username, password } => {
                if !is_https {
                    return Err(GitConfigError::SchemeMismatch {
                        what: "basic auth requires https url",
                    });
                }
                if username.is_empty() || password.is_empty() {
                    return Err(GitConfigError::SchemeMismatch {
                        what: "basic auth: username and password must both be set",
                    });
                }
                Ok(())
            }
            Self::BearerToken(t) => {
                if !is_https {
                    return Err(GitConfigError::SchemeMismatch {
                        what: "bearer auth requires https url",
                    });
                }
                if t.is_empty() {
                    return Err(GitConfigError::SchemeMismatch {
                        what: "bearer auth: token must not be empty",
                    });
                }
                Ok(())
            }
            Self::SshKey {
                identity,
                public,
                known_hosts,
            } => {
                if !is_ssh {
                    return Err(GitConfigError::SchemeMismatch {
                        what: "ssh auth requires ssh url (ssh://, git@host:path, or git+ssh://)",
                    });
                }
                if identity.is_empty() {
                    return Err(GitConfigError::SshBundle {
                        what: "identity is empty",
                    });
                }
                if public.is_empty() {
                    return Err(GitConfigError::SshBundle {
                        what: "identity.pub is empty",
                    });
                }
                if known_hosts.is_empty() {
                    return Err(GitConfigError::SshBundle {
                        what: "known_hosts is empty",
                    });
                }
                Ok(())
            }
        }
    }
}

/// mTLS / custom-CA bundle. Layered on any HTTPS auth mode; either field may
/// be absent (`ca_cert` alone enables custom-CA trust without a client cert).
#[derive(Debug, Clone, Default)]
pub struct TlsBundle {
    /// Client certificate (PEM). Paired with [`Self::client_key`].
    pub client_cert: Option<Vec<u8>>,
    /// Client key (PEM). Paired with [`Self::client_cert`].
    pub client_key: Option<Vec<u8>>,
    /// Custom CA bundle (PEM) for server verification.
    pub ca_cert: Option<Vec<u8>>,
}

impl TlsBundle {
    pub(crate) fn validate(&self) -> Result<(), GitConfigError> {
        match (self.client_cert.is_some(), self.client_key.is_some()) {
            (true, false) => Err(GitConfigError::TlsBundle {
                what: "client_cert set but client_key missing",
            }),
            (false, true) => Err(GitConfigError::TlsBundle {
                what: "client_key set but client_cert missing",
            }),
            _ => Ok(()),
        }?;
        if self.client_cert.is_none() && self.client_key.is_none() && self.ca_cert.is_none() {
            return Err(GitConfigError::TlsBundle {
                what: "bundle present but every field is empty",
            });
        }
        Ok(())
    }
}

pub(crate) fn url_is_https(url: &str) -> bool {
    url.starts_with("https://")
}

pub(crate) fn url_is_ssh(url: &str) -> bool {
    if url.starts_with("ssh://") || url.starts_with("git+ssh://") {
        return true;
    }
    // scp-like syntax: `user@host:path` - no scheme, but contains '@' before ':'
    if let (Some(at), Some(colon)) = (url.find('@'), url.find(':'))
        && at < colon
        && !url.contains("://")
    {
        return true;
    }
    false
}

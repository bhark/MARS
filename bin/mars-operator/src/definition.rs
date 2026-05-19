//! Compose a concrete [`DefinitionSource`] adapter from a `MarsService`
//! `DefinitionSpec`.
//!
//! Single composition site for the four definition-source adapters; mirrors
//! the posture of `mars-bin-shared::build_store_and_publisher`. Variant
//! selection enforces exactly-one (already validated at admission, but
//! re-checked here so the function is total). Credentialed variants
//! (`gitRef`, `s3Ref`) fan out into a same-namespace `Secret` lookup whose
//! key bundle maps to one of the documented auth modes (Flux-source
//! convention).

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::core::v1::Secret;
use kube::api::Api;
use mars_config::units;
use mars_definition_source::DefinitionSource;
use mars_definition_source_configmap::ConfigMapDefinitionSource;
use mars_definition_source_git::{GitAuth, GitConfigError, GitDefinitionSource, GitReference, TlsBundle};
use mars_definition_source_inline::InlineDefinitionSource;
use mars_definition_source_s3::{S3ConfigError, S3Credentials, S3DefinitionSource};

use crate::crd::definition::{DefinitionSpec, GitRef, GitRevision, S3Ref};

/// Errors raised while translating a CRD `DefinitionSpec` into a live adapter.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ResolveError {
    #[error("spec.definition must set exactly one of inline / configMapRef / gitRef / s3Ref (got {got})")]
    ExactlyOneVariant { got: usize },

    #[error("git reference must set exactly one of branch / tag / commit")]
    ExactlyOneGitRevision,

    #[error("secret '{name}' not found in namespace")]
    SecretNotFound { name: String },

    #[error("secret '{secret}' is missing required key '{field}'")]
    SecretFieldMissing { secret: String, field: String },

    #[error("secret '{secret}' key '{field}' is not valid utf-8")]
    SecretFieldNotUtf8 { secret: String, field: String },

    #[error(
        "secret '{secret}': conflicting git auth modes; supply exactly one of (identity+identity.pub+known_hosts) / (username+password) / bearerToken"
    )]
    ConflictingGitAuthModes { secret: String },

    #[error("secret '{secret}': s3 credentials require both accessKey and secretKey when set")]
    IncompleteS3Credentials { secret: String },

    #[error("invalid interval '{input}': {message}")]
    InvalidInterval { input: String, message: String },

    #[error("kube error: {0}")]
    Kube(#[from] kube::Error),

    #[error("git adapter config: {0}")]
    GitConfig(#[from] GitConfigError),

    #[error("s3 adapter config: {0}")]
    S3Config(#[from] S3ConfigError),
}

/// Translate a `MarsService.spec.definition` block + the operator's kube
/// client into a `Box<dyn DefinitionSource>`. Credentialed variants
/// (`gitRef.secretRef`, `s3Ref.secretRef`) read the referenced `Secret` from
/// `ns` and decode its key bundle into an auth mode before constructing the
/// adapter. The configmap variant doesn't need to pre-fetch - its adapter
/// owns kube I/O internally.
pub(crate) async fn resolve(
    spec: &DefinitionSpec,
    ns: &str,
    kube: &kube::Client,
) -> Result<Box<dyn DefinitionSource>, ResolveError> {
    let got = spec.variants_set();
    if got != 1 {
        return Err(ResolveError::ExactlyOneVariant { got });
    }

    if let Some(cm) = &spec.config_map_ref {
        return Ok(Box::new(ConfigMapDefinitionSource::new(
            kube.clone(),
            ns.to_string(),
            cm.name.clone(),
            cm.key.clone(),
        )));
    }

    let bundle = fetch_secret_bundle(spec, ns, kube).await?;
    resolve_from_resolved(spec, bundle)
}

/// Pure dispatch over a pre-resolved secret bundle. Split out from
/// [`resolve`] so unit tests can exercise every credential decision branch
/// without spinning a kube control plane. The configmap branch is handled
/// in [`resolve`] directly (its adapter is built from `(kube, ns, name,
/// key)`, none of which the pure half can supply meaningfully).
pub(crate) fn resolve_from_resolved(
    spec: &DefinitionSpec,
    bundle: Option<ResolvedSecret>,
) -> Result<Box<dyn DefinitionSource>, ResolveError> {
    let got = spec.variants_set();
    if got != 1 {
        return Err(ResolveError::ExactlyOneVariant { got });
    }

    if let Some(payload) = &spec.inline {
        return Ok(Box::new(InlineDefinitionSource::new(payload.clone().into_bytes())));
    }
    if let Some(git_ref) = &spec.git_ref {
        return build_git_adapter(git_ref, bundle.as_ref());
    }
    if let Some(s3_ref) = &spec.s3_ref {
        return build_s3_adapter(s3_ref, bundle.as_ref());
    }
    // configMapRef is handled in resolve() and cannot reach here in production;
    // exposed in tests as ExactlyOneVariant-friendly behaviour by leaving the
    // configmap variant out of pure-dispatch inputs.
    Err(ResolveError::ExactlyOneVariant { got })
}

/// Resolved secret payload paired with its source name for error context.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedSecret {
    pub(crate) name: String,
    pub(crate) data: BTreeMap<String, Vec<u8>>,
}

async fn fetch_secret_bundle(
    spec: &DefinitionSpec,
    ns: &str,
    kube: &kube::Client,
) -> Result<Option<ResolvedSecret>, ResolveError> {
    let secret_name = spec
        .git_ref
        .as_ref()
        .and_then(|g| g.secret_ref.as_ref().map(|r| &r.name))
        .or_else(|| {
            spec.s3_ref
                .as_ref()
                .and_then(|s| s.secret_ref.as_ref().map(|r| &r.name))
        });
    let Some(name) = secret_name else {
        return Ok(None);
    };
    let api: Api<Secret> = Api::namespaced(kube.clone(), ns);
    let secret = api
        .get_opt(name)
        .await?
        .ok_or_else(|| ResolveError::SecretNotFound { name: name.clone() })?;
    let data = secret
        .data
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| (k, v.0))
        .collect();
    Ok(Some(ResolvedSecret {
        name: name.clone(),
        data,
    }))
}

fn build_git_adapter(
    git_ref: &GitRef,
    bundle: Option<&ResolvedSecret>,
) -> Result<Box<dyn DefinitionSource>, ResolveError> {
    let (auth, tls) = git_auth_from_bundle(bundle, git_ref.secret_ref.is_some())?;
    let reference = git_reference(&git_ref.git_ref)?;
    let interval = parse_optional_interval(git_ref.interval.as_deref())?;
    let adapter = GitDefinitionSource::new(
        git_ref.url.clone(),
        reference,
        git_ref.path.clone(),
        interval,
        auth,
        tls,
    )?;
    Ok(Box::new(adapter))
}

fn build_s3_adapter(
    s3_ref: &S3Ref,
    bundle: Option<&ResolvedSecret>,
) -> Result<Box<dyn DefinitionSource>, ResolveError> {
    let credentials = s3_credentials_from_bundle(bundle, s3_ref.secret_ref.is_some())?;
    let interval = parse_optional_interval(s3_ref.interval.as_deref())?;
    let adapter = S3DefinitionSource::new(
        s3_ref.endpoint.clone(),
        s3_ref.region.clone(),
        s3_ref.bucket.clone(),
        s3_ref.key.clone(),
        interval,
        credentials,
    )?;
    Ok(Box::new(adapter))
}

fn git_reference(rev: &GitRevision) -> Result<GitReference, ResolveError> {
    let count = usize::from(rev.branch.is_some()) + usize::from(rev.tag.is_some()) + usize::from(rev.commit.is_some());
    if count != 1 {
        return Err(ResolveError::ExactlyOneGitRevision);
    }
    if let Some(b) = &rev.branch {
        return Ok(GitReference::Branch(b.clone()));
    }
    if let Some(t) = &rev.tag {
        return Ok(GitReference::Tag(t.clone()));
    }
    if let Some(c) = &rev.commit {
        return Ok(GitReference::Commit(c.clone()));
    }
    unreachable!("count == 1 but no field set")
}

// well-known secret keys; mirror source.toolkit.fluxcd.io/v1.GitRepository.
const GIT_KEY_IDENTITY: &str = "identity";
const GIT_KEY_IDENTITY_PUB: &str = "identity.pub";
const GIT_KEY_KNOWN_HOSTS: &str = "known_hosts";
const GIT_KEY_USERNAME: &str = "username";
const GIT_KEY_PASSWORD: &str = "password";
const GIT_KEY_BEARER: &str = "bearerToken";
const TLS_KEY_CERT: &str = "tls.crt";
const TLS_KEY_KEY: &str = "tls.key";
const TLS_KEY_CA: &str = "ca.crt";

// s3 bundle keys; mirror source.toolkit.fluxcd.io/v1beta2.Bucket.
const S3_KEY_ACCESS: &str = "accessKey";
const S3_KEY_SECRET: &str = "secretKey";
const S3_KEY_SESSION: &str = "sessionToken";

fn git_auth_from_bundle(
    bundle: Option<&ResolvedSecret>,
    secret_ref_set: bool,
) -> Result<(GitAuth, Option<TlsBundle>), ResolveError> {
    let Some(bundle) = bundle else {
        debug_assert!(
            !secret_ref_set,
            "fetch_secret_bundle should have produced Some when secret_ref is set"
        );
        return Ok((GitAuth::None, None));
    };
    let data = &bundle.data;
    let has_ssh = data.contains_key(GIT_KEY_IDENTITY)
        || data.contains_key(GIT_KEY_IDENTITY_PUB)
        || data.contains_key(GIT_KEY_KNOWN_HOSTS);
    let has_basic = data.contains_key(GIT_KEY_USERNAME) || data.contains_key(GIT_KEY_PASSWORD);
    let has_bearer = data.contains_key(GIT_KEY_BEARER);

    let modes = usize::from(has_ssh) + usize::from(has_basic) + usize::from(has_bearer);
    if modes > 1 {
        return Err(ResolveError::ConflictingGitAuthModes {
            secret: bundle.name.clone(),
        });
    }

    let auth = if has_ssh {
        GitAuth::SshKey {
            identity: read_required_bytes(data, &bundle.name, GIT_KEY_IDENTITY)?,
            public: read_required_bytes(data, &bundle.name, GIT_KEY_IDENTITY_PUB)?,
            known_hosts: read_required_bytes(data, &bundle.name, GIT_KEY_KNOWN_HOSTS)?,
        }
    } else if has_basic {
        GitAuth::BasicAuth {
            username: read_required_string(data, &bundle.name, GIT_KEY_USERNAME)?,
            password: read_required_string(data, &bundle.name, GIT_KEY_PASSWORD)?,
        }
    } else if has_bearer {
        GitAuth::BearerToken(read_required_string(data, &bundle.name, GIT_KEY_BEARER)?)
    } else {
        // bundle present, no auth keys; legitimate when only mTLS / custom CA is in use
        GitAuth::None
    };

    let tls = build_tls_bundle(data);
    Ok((auth, tls))
}

fn build_tls_bundle(data: &BTreeMap<String, Vec<u8>>) -> Option<TlsBundle> {
    let cert = data.get(TLS_KEY_CERT).cloned();
    let key = data.get(TLS_KEY_KEY).cloned();
    let ca = data.get(TLS_KEY_CA).cloned();
    if cert.is_none() && key.is_none() && ca.is_none() {
        return None;
    }
    Some(TlsBundle {
        client_cert: cert,
        client_key: key,
        ca_cert: ca,
    })
}

fn s3_credentials_from_bundle(
    bundle: Option<&ResolvedSecret>,
    secret_ref_set: bool,
) -> Result<Option<S3Credentials>, ResolveError> {
    let Some(bundle) = bundle else {
        debug_assert!(
            !secret_ref_set,
            "fetch_secret_bundle should have produced Some when secret_ref is set"
        );
        return Ok(None);
    };
    let data = &bundle.data;
    let access = data.get(S3_KEY_ACCESS);
    let secret = data.get(S3_KEY_SECRET);
    match (access, secret) {
        (Some(_), Some(_)) => Ok(Some(S3Credentials {
            access_key: read_required_string(data, &bundle.name, S3_KEY_ACCESS)?,
            secret_key: read_required_string(data, &bundle.name, S3_KEY_SECRET)?,
            session_token: match data.get(S3_KEY_SESSION) {
                Some(_) => Some(read_required_string(data, &bundle.name, S3_KEY_SESSION)?),
                None => None,
            },
        })),
        _ => Err(ResolveError::IncompleteS3Credentials {
            secret: bundle.name.clone(),
        }),
    }
}

fn read_required_bytes(
    data: &BTreeMap<String, Vec<u8>>,
    secret_name: &str,
    field: &str,
) -> Result<Vec<u8>, ResolveError> {
    data.get(field)
        .cloned()
        .ok_or_else(|| ResolveError::SecretFieldMissing {
            secret: secret_name.into(),
            field: field.into(),
        })
}

fn read_required_string(
    data: &BTreeMap<String, Vec<u8>>,
    secret_name: &str,
    field: &str,
) -> Result<String, ResolveError> {
    let bytes = read_required_bytes(data, secret_name, field)?;
    String::from_utf8(bytes).map_err(|_| ResolveError::SecretFieldNotUtf8 {
        secret: secret_name.into(),
        field: field.into(),
    })
}

fn parse_optional_interval(input: Option<&str>) -> Result<Option<Duration>, ResolveError> {
    match input {
        Some(s) => units::parse_duration(s)
            .map(Some)
            .map_err(|e| ResolveError::InvalidInterval {
                input: s.into(),
                message: e.to_string(),
            }),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests;

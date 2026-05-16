//! Compose an admin DSN from a component-style Secret (CNPG / Zalando / Crunchy
//! shape) plus optional host/port/database fallbacks parsed out of the
//! service's `spec.config.sources[].dsn`.
//!
//! The composed string is persisted by the reconciler into a managed
//! `<svc>-bootstrap-admin-credentials` Secret (owner-ref to the MarsService),
//! and the bootstrap/teardown Job projects it via `secretKeyRef` as
//! `MARS_ADMIN_DSN`. The DSN therefore never lands on the Job spec.

use std::collections::BTreeMap;

use crate::crd::AdminCredentialsRef;

#[derive(Debug, thiserror::Error)]
pub(crate) enum DsnError {
    #[error("admin credentials Secret '{name}' is missing key '{key}'")]
    MissingKey { name: String, key: String },

    #[error("admin credentials Secret '{name}' key '{key}' is not valid UTF-8")]
    NotUtf8 { name: String, key: String },

    #[error(
        "host could not be resolved: not in adminCredentialsRef.hostKey and not parseable from spec.config.source.dsn"
    )]
    HostUnresolved,

    #[error(
        "database could not be resolved: not in adminCredentialsRef.databaseKey and not parseable from spec.config.source.dsn"
    )]
    DatabaseUnresolved,
}

#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct DsnComponents {
    pub(crate) host: Option<String>,
    pub(crate) port: Option<String>,
    pub(crate) database: Option<String>,
    /// Raw query string from the source DSN ("sslmode=verify-full&..."), kept
    /// verbatim so TLS / connection options carry across to the admin DSN.
    pub(crate) query: Option<String>,
}

/// Parse the libpq URI form (`postgresql://...` / `postgres://...`) just far
/// enough to extract host, port, database, and query. Templated values
/// (`${VAR}`) are replaced with a sentinel so the URI still parses; the
/// sentinel is rejected by the caller (treated as "not set") rather than
/// returned as a literal.
pub(crate) fn parse_dsn_components(dsn: &str) -> DsnComponents {
    let sanitised = strip_placeholders(dsn);
    let Some(after_scheme) = sanitised
        .strip_prefix("postgresql://")
        .or_else(|| sanitised.strip_prefix("postgres://"))
    else {
        return DsnComponents::default();
    };
    let (before_query, query) = match after_scheme.split_once('?') {
        Some((p, q)) => (p, Some(q.to_string())),
        None => (after_scheme, None),
    };
    let (authority, dbname) = match before_query.split_once('/') {
        Some((a, p)) => (a, accept_non_sentinel(p)),
        None => (before_query, None),
    };
    let host_port = authority.rsplit_once('@').map(|(_, hp)| hp).unwrap_or(authority);
    let (host, port) = split_host_port(host_port);
    DsnComponents {
        host: accept_non_sentinel(&host),
        port: port.and_then(|p| accept_non_sentinel(&p)),
        database: dbname,
        query,
    }
}

/// Compose a libpq URI from a component-style Secret. Host/port/database fall
/// back to whatever the optional `fallback` carries when the corresponding
/// key is not set on the `AdminCredentialsRef`.
pub(crate) fn compose_admin_dsn(
    creds: &AdminCredentialsRef,
    secret_data: &BTreeMap<String, Vec<u8>>,
    fallback: &DsnComponents,
) -> Result<String, DsnError> {
    let username = read_required_key(secret_data, &creds.secret_name, &creds.username_key)?;
    let password = read_required_key(secret_data, &creds.secret_name, &creds.password_key)?;
    let host = read_optional_key(secret_data, &creds.secret_name, creds.host_key.as_deref())?
        .or_else(|| fallback.host.clone())
        .ok_or(DsnError::HostUnresolved)?;
    let port = read_optional_key(secret_data, &creds.secret_name, creds.port_key.as_deref())?
        .or_else(|| fallback.port.clone());
    let database = read_optional_key(secret_data, &creds.secret_name, creds.database_key.as_deref())?
        .or_else(|| fallback.database.clone())
        .ok_or(DsnError::DatabaseUnresolved)?;

    let mut out = String::from("postgresql://");
    out.push_str(&percent_encode(&username));
    out.push(':');
    out.push_str(&percent_encode(&password));
    out.push('@');
    out.push_str(&host);
    if let Some(p) = port {
        out.push(':');
        out.push_str(&p);
    }
    out.push('/');
    out.push_str(&percent_encode(&database));
    if let Some(q) = &fallback.query
        && !q.is_empty()
    {
        out.push('?');
        out.push_str(q);
    }
    Ok(out)
}

fn read_required_key(data: &BTreeMap<String, Vec<u8>>, secret_name: &str, key: &str) -> Result<String, DsnError> {
    let bytes = data.get(key).ok_or_else(|| DsnError::MissingKey {
        name: secret_name.into(),
        key: key.into(),
    })?;
    String::from_utf8(bytes.clone()).map_err(|_| DsnError::NotUtf8 {
        name: secret_name.into(),
        key: key.into(),
    })
}

fn read_optional_key(
    data: &BTreeMap<String, Vec<u8>>,
    secret_name: &str,
    key: Option<&str>,
) -> Result<Option<String>, DsnError> {
    match key {
        Some(k) => read_required_key(data, secret_name, k).map(Some),
        None => Ok(None),
    }
}

/// percent-encode the small subset of URI-reserved characters that can appear
/// in usernames, passwords, and dbnames. Keep this in lock-step with what
/// `tokio_postgres::Config::from_str` accepts on the consuming side.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let safe = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        if safe {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

const SENTINEL: &str = "__MARS_OPERATOR_PLACEHOLDER__";

fn accept_non_sentinel(s: &str) -> Option<String> {
    if s.is_empty() || s.contains(SENTINEL) {
        None
    } else {
        Some(s.to_string())
    }
}

/// Replace `${VAR}` / `${VAR:-default}` tokens with a sentinel so a templated
/// DSN still parses. Mirrors the parser in `crate::config::strip_placeholders`
/// but emits a private sentinel we recognise on the way out.
fn strip_placeholders(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'$' {
            out.push('$');
            i += 2;
            continue;
        }
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'}' {
                j += 1;
            }
            if j < bytes.len() {
                out.push_str(SENTINEL);
                i = j + 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn split_host_port(s: &str) -> (String, Option<String>) {
    if let Some(stripped) = s.strip_prefix('[') {
        // ipv6 literal: [::1] or [::1]:5432
        if let Some(end) = stripped.find(']') {
            let host = &stripped[..end];
            let rest = &stripped[end + 1..];
            let port = rest.strip_prefix(':').filter(|s| !s.is_empty()).map(String::from);
            return (host.to_string(), port);
        }
    }
    match s.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), Some(p.to_string()).filter(|s| !s.is_empty())),
        None => (s.to_string(), None),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn secret(entries: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
        entries
            .iter()
            .map(|(k, v)| ((*k).into(), v.as_bytes().to_vec()))
            .collect()
    }

    fn cnpg_default_creds(secret_name: &str) -> AdminCredentialsRef {
        AdminCredentialsRef {
            secret_name: secret_name.into(),
            username_key: "username".into(),
            password_key: "password".into(),
            host_key: None,
            port_key: None,
            database_key: None,
        }
    }

    #[test]
    fn parse_extracts_host_port_database_from_uri() {
        let c = parse_dsn_components("postgresql://user:pw@db.example:5432/maps?sslmode=require");
        assert_eq!(c.host.as_deref(), Some("db.example"));
        assert_eq!(c.port.as_deref(), Some("5432"));
        assert_eq!(c.database.as_deref(), Some("maps"));
        assert_eq!(c.query.as_deref(), Some("sslmode=require"));
    }

    #[test]
    fn parse_skips_templated_userinfo() {
        let c = parse_dsn_components("postgres://${MARS_RUNTIME_PASSWORD}@db.example:5432/maps");
        assert_eq!(c.host.as_deref(), Some("db.example"));
        assert_eq!(c.port.as_deref(), Some("5432"));
        assert_eq!(c.database.as_deref(), Some("maps"));
    }

    #[test]
    fn parse_returns_none_for_pure_placeholder() {
        let c = parse_dsn_components("${PG_DSN}");
        assert_eq!(c.host, None);
        assert_eq!(c.port, None);
        assert_eq!(c.database, None);
    }

    #[test]
    fn parse_handles_ipv6_literal() {
        let c = parse_dsn_components("postgresql://user:pw@[::1]:5432/maps");
        assert_eq!(c.host.as_deref(), Some("::1"));
        assert_eq!(c.port.as_deref(), Some("5432"));
    }

    #[test]
    fn compose_uses_fallback_for_missing_host_port_database() {
        let creds = cnpg_default_creds("cnpg-superuser");
        let data = secret(&[("username", "postgres"), ("password", "s3cret")]);
        let fallback = parse_dsn_components("postgresql://placeholder@db.svc:6432/maps?sslmode=require");
        let dsn = compose_admin_dsn(&creds, &data, &fallback).unwrap();
        assert_eq!(dsn, "postgresql://postgres:s3cret@db.svc:6432/maps?sslmode=require");
    }

    #[test]
    fn compose_prefers_explicit_keys_over_fallback() {
        let creds = AdminCredentialsRef {
            secret_name: "cnpg-superuser".into(),
            username_key: "username".into(),
            password_key: "password".into(),
            host_key: Some("host".into()),
            port_key: Some("port".into()),
            database_key: Some("database".into()),
        };
        let data = secret(&[
            ("username", "postgres"),
            ("password", "s3cret"),
            ("host", "explicit.svc"),
            ("port", "5433"),
            ("database", "explicit_db"),
        ]);
        let fallback = parse_dsn_components("postgresql://x@fallback.svc:5432/fallback_db");
        let dsn = compose_admin_dsn(&creds, &data, &fallback).unwrap();
        assert_eq!(dsn, "postgresql://postgres:s3cret@explicit.svc:5433/explicit_db");
    }

    #[test]
    fn compose_percent_encodes_special_characters_in_password() {
        let creds = cnpg_default_creds("s");
        let data = secret(&[("username", "user@x"), ("password", "p@ss/word")]);
        let fallback = parse_dsn_components("postgresql://x@db/maps");
        let dsn = compose_admin_dsn(&creds, &data, &fallback).unwrap();
        assert!(dsn.starts_with("postgresql://user%40x:p%40ss%2Fword@"));
    }

    #[test]
    fn compose_errors_when_password_key_missing() {
        let creds = cnpg_default_creds("s");
        let data = secret(&[("username", "user")]);
        let fallback = parse_dsn_components("postgresql://x@db/maps");
        let err = compose_admin_dsn(&creds, &data, &fallback).unwrap_err();
        match err {
            DsnError::MissingKey { name, key } => {
                assert_eq!(name, "s");
                assert_eq!(key, "password");
            }
            other => panic!("expected MissingKey, got {other:?}"),
        }
    }

    #[test]
    fn compose_errors_when_host_unresolvable() {
        let creds = cnpg_default_creds("s");
        let data = secret(&[("username", "u"), ("password", "p")]);
        let fallback = parse_dsn_components("${PG_DSN}");
        let err = compose_admin_dsn(&creds, &data, &fallback).unwrap_err();
        assert!(matches!(err, DsnError::HostUnresolved));
    }

    #[test]
    fn compose_errors_when_database_unresolvable() {
        let creds = cnpg_default_creds("s");
        let data = secret(&[("username", "u"), ("password", "p")]);
        let fallback = parse_dsn_components("postgresql://x@db.svc:5432/");
        let err = compose_admin_dsn(&creds, &data, &fallback).unwrap_err();
        assert!(matches!(err, DsnError::DatabaseUnresolved));
    }
}

#![allow(clippy::unwrap_used, clippy::panic)]

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

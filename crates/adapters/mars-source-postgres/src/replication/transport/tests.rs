#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn cfg(dsn: &str) -> PgConfig {
    PgConfig {
        dsn: dsn.into(),
        publication: "p".into(),
        slot: "s".into(),
        ..Default::default()
    }
}

#[test]
fn build_config_parses_uri_dsn() {
    let r = build_replication_config(&cfg("postgres://alice:secret@db.example:6543/forv")).unwrap();
    assert_eq!(r.host, "db.example");
    assert_eq!(r.port, 6543);
    assert_eq!(r.user, "alice");
    assert_eq!(r.password, "secret");
    assert_eq!(r.database, "forv");
    assert_eq!(r.slot, "s");
    assert_eq!(r.publication, "p");
}

#[test]
fn build_config_maps_sslmode_disable() {
    let r = build_replication_config(&cfg("postgres://u:p@h/d?sslmode=disable")).unwrap();
    assert_eq!(r.tls.mode, PgwireSslMode::Disable);
}

#[test]
fn build_config_maps_sslmode_require() {
    let r = build_replication_config(&cfg("postgres://u:p@h/d?sslmode=require")).unwrap();
    assert_eq!(r.tls.mode, PgwireSslMode::Require);
}

#[test]
fn build_config_rejects_missing_user() {
    let err = build_replication_config(&cfg("postgres://h/d")).unwrap_err();
    match err {
        SourceError::Backend { source, .. } => {
            let m = source.to_string();
            assert!(m.contains("user"), "msg = {m}");
        }
        other => panic!("expected Backend, got {other:?}"),
    }
}

#[test]
fn build_config_rejects_missing_dbname() {
    let err = build_replication_config(&cfg("postgres://u:p@h/")).unwrap_err();
    match err {
        SourceError::Backend { source, .. } => {
            let m = source.to_string();
            assert!(m.contains("dbname"), "msg = {m}");
        }
        other => panic!("expected Backend, got {other:?}"),
    }
}

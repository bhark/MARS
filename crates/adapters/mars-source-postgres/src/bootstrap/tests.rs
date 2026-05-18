#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn plan() -> BootstrapPlan {
    BootstrapPlan {
        role: "mars_replicator".into(),
        runtime_password: "s3cret".into(),
        publication: "mars_pub".into(),
        slot: "mars_slot".into(),
        schemas: vec!["public".into(), "geo".into()],
    }
}

#[test]
fn renders_role_in_do_block() {
    let s = render_statements(&plan()).unwrap();
    assert!(s[0].contains("CREATE ROLE \"mars_replicator\""));
    assert!(s[0].contains("ALTER ROLE \"mars_replicator\""));
    assert!(s[0].contains("WITH LOGIN REPLICATION PASSWORD 's3cret'"));
}

#[test]
fn renders_grants_per_schema() {
    let s = render_statements(&plan()).unwrap();
    let joined = s.join("\n");
    assert!(joined.contains("GRANT USAGE ON SCHEMA \"public\" TO \"mars_replicator\";"));
    assert!(joined.contains("GRANT USAGE ON SCHEMA \"geo\" TO \"mars_replicator\";"));
    assert!(joined.contains("GRANT SELECT ON ALL TABLES IN SCHEMA \"public\" TO \"mars_replicator\";"));
    assert!(
        joined.contains("ALTER DEFAULT PRIVILEGES IN SCHEMA \"geo\" GRANT SELECT ON TABLES TO \"mars_replicator\";")
    );
}

#[test]
fn renders_publication_for_tables_in_schema() {
    let s = render_statements(&plan()).unwrap();
    let last = s.last().unwrap();
    assert_eq!(
        last,
        "CREATE PUBLICATION \"mars_pub\" FOR TABLES IN SCHEMA \"public\", \"geo\";"
    );
}

#[test]
fn renders_slot_creation_separately() {
    assert_eq!(
        render_slot_creation(&plan()),
        "SELECT pg_create_logical_replication_slot('mars_slot', 'pgoutput');"
    );
}

#[test]
fn escapes_password_with_quote() {
    let mut p = plan();
    p.runtime_password = "it's a secret".into();
    let s = render_statements(&p).unwrap();
    assert!(s[0].contains("PASSWORD 'it''s a secret'"));
}

#[test]
fn teardown_emits_only_requested_drops() {
    let plan = TeardownPlan {
        role: "mars_replicator".into(),
        publication: "mars_pub".into(),
        slot: "mars_slot".into(),
        drop_slot: true,
        drop_publication: false,
        drop_role: false,
    };
    let s = render_teardown_statements(&plan).unwrap();
    assert_eq!(s.len(), 1);
    assert!(s[0].contains("pg_drop_replication_slot('mars_slot')"));
}

#[test]
fn teardown_order_is_slot_publication_role() {
    let plan = TeardownPlan {
        role: "mars_replicator".into(),
        publication: "mars_pub".into(),
        slot: "mars_slot".into(),
        drop_slot: true,
        drop_publication: true,
        drop_role: true,
    };
    let s = render_teardown_statements(&plan).unwrap();
    assert_eq!(s.len(), 3);
    assert!(s[0].contains("pg_drop_replication_slot"));
    assert!(s[1].contains("DROP PUBLICATION"));
    assert!(s[2].contains("DROP ROLE"));
}

#[test]
fn rejects_dotted_identifier() {
    let mut p = plan();
    p.schemas = vec!["a.b".into()];
    let err = render_statements(&p).unwrap_err();
    assert!(matches!(err, BootstrapError::Identifier(_)));
}

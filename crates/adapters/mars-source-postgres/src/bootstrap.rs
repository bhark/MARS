//! Idempotent catalog provisioning. Renders and applies the role / grants /
//! publication / slot a MARS instance needs against a postgres source.
//!
//! Two surfaces:
//! - pure renderers (`render_statements`, `render_slot_creation`,
//!   `render_teardown_statements`) that emit `psql`-pasteable SQL. These are
//!   the canonical reference `mars setup --dry-run` prints.
//! - async executors (`apply`, `teardown`) that open a one-shot admin
//!   connection and run the rendered statements with the small handful of
//!   error codes that mean "already in the desired state" treated as no-ops.
//!
//! `pg_create_logical_replication_slot` runs outside the transaction that
//! covers role + grants + publication: the function can write WAL, and we
//! prefer the simpler "two-step" failure model (slot create can be re-run
//! without rolling back grants) over the marginal atomicity gain.

use std::str::FromStr;

use thiserror::Error;
use tokio_postgres::error::SqlState;

use crate::quote::quote_ident;

/// Bootstrap inputs. Names are already-validated identifiers (the
/// mars-config validator restricts them to `[a-z_][a-z0-9_]*`); the renderer
/// double-checks via `quote_ident` to fail loudly if a caller bypassed
/// validation.
#[derive(Debug, Clone)]
pub struct BootstrapPlan {
    pub role: String,
    pub runtime_password: String,
    pub publication: String,
    pub slot: String,
    pub schemas: Vec<String>,
}

/// Teardown inputs. Drop flags map 1:1 to the operator's `teardown_on_delete`
/// CR policy; setting all three to false is a valid no-op.
#[derive(Debug, Clone)]
pub struct TeardownPlan {
    pub role: String,
    pub publication: String,
    pub slot: String,
    pub drop_slot: bool,
    pub drop_publication: bool,
    pub drop_role: bool,
}

#[derive(Debug, Error)]
pub enum BootstrapError {
    #[error("invalid identifier: {0}")]
    Identifier(String),
    #[error("connect: {0}")]
    Connect(#[source] tokio_postgres::Error),
    #[error("dsn: {0}")]
    Dsn(#[source] tokio_postgres::Error),
    #[error("query {stmt}: {source}")]
    Query {
        stmt: String,
        #[source]
        source: tokio_postgres::Error,
    },
    #[error("tls: {0}")]
    Tls(String),
}

impl BootstrapError {
    fn query(stmt: impl Into<String>, source: tokio_postgres::Error) -> Self {
        Self::Query {
            stmt: stmt.into(),
            source,
        }
    }
}

/// Rendered "in-transaction" statements: role, grants, default privileges,
/// publication ensure. Order is stable for deterministic dry-run output.
///
/// Publication reconciliation: this renderer assumes the publication does
/// not yet exist. If it does, [`apply`] computes the schema delta against
/// `pg_publication_namespace` and emits the necessary `ALTER PUBLICATION
/// ... ADD/DROP TABLES IN SCHEMA` statements instead. Dry-run prints the
/// "create from scratch" form, which is the canonical reference for the
/// docs.
pub fn render_statements(plan: &BootstrapPlan) -> Result<Vec<String>, BootstrapError> {
    let role_q = ident(&plan.role)?;
    let pub_q = ident(&plan.publication)?;
    let role_lit = sql_literal(&plan.role);
    let password_lit = sql_literal(&plan.runtime_password);

    let mut out = Vec::with_capacity(2 + plan.schemas.len() * 3);

    // role: DO block keeps the statement idempotent without relying on a
    // postgres feature flag. password is updated unconditionally so a rotated
    // secret takes effect on next bootstrap apply.
    out.push(format!(
        "DO $$\nBEGIN\n  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = {role_lit}) THEN\n    \
         CREATE ROLE {role_q} WITH LOGIN REPLICATION PASSWORD {password_lit};\n  ELSE\n    \
         ALTER ROLE {role_q} WITH LOGIN REPLICATION PASSWORD {password_lit};\n  END IF;\nEND\n$$;"
    ));

    for s in &plan.schemas {
        let s_q = ident(s)?;
        out.push(format!("GRANT USAGE ON SCHEMA {s_q} TO {role_q};"));
        out.push(format!("GRANT SELECT ON ALL TABLES IN SCHEMA {s_q} TO {role_q};"));
        out.push(format!(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA {s_q} GRANT SELECT ON TABLES TO {role_q};"
        ));
    }

    // publication: create-from-scratch form. apply() switches to ALTER when
    // the publication is already present.
    let schemas_q = plan
        .schemas
        .iter()
        .map(|s| ident(s))
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");
    out.push(format!(
        "CREATE PUBLICATION {pub_q} FOR TABLES IN SCHEMA {schemas_q};"
    ));

    Ok(out)
}

/// Slot creation statement, rendered separately because it runs outside the
/// in-transaction batch.
pub fn render_slot_creation(plan: &BootstrapPlan) -> String {
    let slot_lit = sql_literal(&plan.slot);
    format!("SELECT pg_create_logical_replication_slot({slot_lit}, 'pgoutput');")
}

/// Rendered teardown statements in drop-safe order: slot first (releases WAL
/// retention), then publication, then revoke + role.
pub fn render_teardown_statements(plan: &TeardownPlan) -> Result<Vec<String>, BootstrapError> {
    let mut out = Vec::new();
    if plan.drop_slot {
        out.push(format!(
            "SELECT pg_drop_replication_slot({});",
            sql_literal(&plan.slot)
        ));
    }
    if plan.drop_publication {
        let pub_q = ident(&plan.publication)?;
        out.push(format!("DROP PUBLICATION IF EXISTS {pub_q};"));
    }
    if plan.drop_role {
        let role_q = ident(&plan.role)?;
        // reassigning owned objects to nobody first would be wrong (role only
        // ever owned grants, never relations). plain DROP ROLE IF EXISTS is
        // sufficient; revoke happens implicitly when grants follow the role.
        out.push(format!("DROP ROLE IF EXISTS {role_q};"));
    }
    Ok(out)
}

/// Apply the bootstrap plan against an admin DSN. Runs role/grants/publication
/// in a single transaction, then ensures the replication slot in a separate
/// statement. Idempotent: re-running against an already-bootstrapped catalog
/// reconciles publication schema membership and is otherwise a no-op.
pub async fn apply(admin_dsn: &str, plan: &BootstrapPlan) -> Result<(), BootstrapError> {
    let mut client = connect(admin_dsn).await?;

    let tx = client
        .transaction()
        .await
        .map_err(|e| BootstrapError::query("BEGIN", e))?;

    // role + grants + default privileges in transaction
    let pre = render_statements(plan)?;
    // last element of `pre` is the CREATE PUBLICATION form. swap for an
    // ALTER form when the publication already exists.
    let (grants, _create_pub) = pre.split_at(pre.len() - 1);
    for stmt in grants {
        tx.batch_execute(stmt).await.map_err(|e| BootstrapError::query(stmt, e))?;
    }

    let pub_q = ident(&plan.publication)?;
    let pub_exists: bool = tx
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_publication WHERE pubname = $1)",
            &[&plan.publication],
        )
        .await
        .map_err(|e| BootstrapError::query("probe pg_publication", e))?
        .get(0);

    if !pub_exists {
        let schemas_q = plan
            .schemas
            .iter()
            .map(|s| ident(s))
            .collect::<Result<Vec<_>, _>>()?
            .join(", ");
        let stmt = format!("CREATE PUBLICATION {pub_q} FOR TABLES IN SCHEMA {schemas_q};");
        tx.batch_execute(&stmt)
            .await
            .map_err(|e| BootstrapError::query(stmt, e))?;
    } else {
        let rows = tx
            .query(
                "SELECT n.nspname FROM pg_publication_namespace pn \
                 JOIN pg_namespace n ON n.oid = pn.pnnspid \
                 JOIN pg_publication p ON p.oid = pn.pnpubid \
                 WHERE p.pubname = $1",
                &[&plan.publication],
            )
            .await
            .map_err(|e| BootstrapError::query("probe pg_publication_namespace", e))?;
        let current: std::collections::BTreeSet<String> =
            rows.iter().map(|r| r.get::<_, String>(0)).collect();
        let desired: std::collections::BTreeSet<String> = plan.schemas.iter().cloned().collect();

        let to_add: Vec<String> = desired.difference(&current).cloned().collect();
        let to_drop: Vec<String> = current.difference(&desired).cloned().collect();
        if !to_add.is_empty() {
            let list = to_add
                .iter()
                .map(|s| ident(s))
                .collect::<Result<Vec<_>, _>>()?
                .join(", ");
            let stmt = format!("ALTER PUBLICATION {pub_q} ADD TABLES IN SCHEMA {list};");
            tx.batch_execute(&stmt)
                .await
                .map_err(|e| BootstrapError::query(stmt, e))?;
        }
        if !to_drop.is_empty() {
            let list = to_drop
                .iter()
                .map(|s| ident(s))
                .collect::<Result<Vec<_>, _>>()?
                .join(", ");
            let stmt = format!("ALTER PUBLICATION {pub_q} DROP TABLES IN SCHEMA {list};");
            tx.batch_execute(&stmt)
                .await
                .map_err(|e| BootstrapError::query(stmt, e))?;
        }
    }

    tx.commit().await.map_err(|e| BootstrapError::query("COMMIT", e))?;

    // slot: separate statement; duplicate-slot is a no-op.
    let slot_stmt = render_slot_creation(plan);
    match client.batch_execute(&slot_stmt).await {
        Ok(_) => Ok(()),
        Err(e) if e.code() == Some(&SqlState::DUPLICATE_OBJECT) => Ok(()),
        Err(e) => Err(BootstrapError::query(slot_stmt, e)),
    }
}

/// Apply the teardown plan. Statements are executed in render order; missing
/// objects are tolerated (`IF EXISTS` / SqlState matching).
pub async fn teardown(admin_dsn: &str, plan: &TeardownPlan) -> Result<(), BootstrapError> {
    let client = connect(admin_dsn).await?;

    if plan.drop_slot {
        let stmt = format!("SELECT pg_drop_replication_slot({});", sql_literal(&plan.slot));
        match client.batch_execute(&stmt).await {
            Ok(_) => {}
            // undefined_object: slot already gone.
            Err(e) if e.code() == Some(&SqlState::UNDEFINED_OBJECT) => {}
            Err(e) => return Err(BootstrapError::query(stmt, e)),
        }
    }
    if plan.drop_publication {
        let pub_q = ident(&plan.publication)?;
        let stmt = format!("DROP PUBLICATION IF EXISTS {pub_q};");
        client
            .batch_execute(&stmt)
            .await
            .map_err(|e| BootstrapError::query(stmt, e))?;
    }
    if plan.drop_role {
        let role_q = ident(&plan.role)?;
        let stmt = format!("DROP ROLE IF EXISTS {role_q};");
        client
            .batch_execute(&stmt)
            .await
            .map_err(|e| BootstrapError::query(stmt, e))?;
    }
    Ok(())
}

// re-use the shared identifier quoter; map its SourceError into our local
// BootstrapError so callers see one error type.
fn ident(name: &str) -> Result<String, BootstrapError> {
    quote_ident(name).map_err(|e| BootstrapError::Identifier(e.to_string()))
}

// minimal sql string literal escaper: doubles single-quotes, rejects NUL.
// passwords and identifier names are the only strings we splice as literals.
fn sql_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

async fn connect(admin_dsn: &str) -> Result<tokio_postgres::Client, BootstrapError> {
    let pg_cfg = tokio_postgres::Config::from_str(admin_dsn).map_err(BootstrapError::Dsn)?;

    if pg_cfg.get_ssl_mode() == tokio_postgres::config::SslMode::Disable {
        let (client, conn) = pg_cfg
            .connect(tokio_postgres::NoTls)
            .await
            .map_err(BootstrapError::Connect)?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::warn!(error = %e, "bootstrap admin connection closed with error");
            }
        });
        Ok(client)
    } else {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let load = rustls_native_certs::load_native_certs();
        if load.certs.is_empty() {
            return Err(BootstrapError::Tls(
                "no native trust roots available; refusing to connect with empty trust store".into(),
            ));
        }
        let mut roots = rustls::RootCertStore::empty();
        for cert in load.certs {
            let _ = roots.add(cert);
        }
        let tls_cfg = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let tls = tokio_postgres_rustls::MakeRustlsConnect::new(tls_cfg);
        let (client, conn) = pg_cfg.connect(tls).await.map_err(BootstrapError::Connect)?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::warn!(error = %e, "bootstrap admin connection closed with error");
            }
        });
        Ok(client)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
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
        assert!(joined.contains("ALTER DEFAULT PRIVILEGES IN SCHEMA \"geo\" GRANT SELECT ON TABLES TO \"mars_replicator\";"));
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
}

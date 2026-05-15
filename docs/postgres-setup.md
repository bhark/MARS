# PostgreSQL setup for the MARS compiler

The compiler streams logical replication via `pgoutput` from the source PostgreSQL instance. This document covers the two ways to provision the catalog state MARS needs (role, grants, publication, slot) and the day-2 diagnostics for either path.

The authoritative contract is the `mars-source-postgres` adapter and its config schema in `mars-config`. Both paths below converge on the same six idempotent SQL operations - `mars setup --dry-run --config <path>` prints them as the canonical reference.

## What MARS needs on the source

For one MARS service (one logical-replication subscription) the source DB must have:

1. `wal_level = logical` cluster-wide. Cluster-level setting; not in scope for `mars setup`.
2. A login role with the `REPLICATION` attribute that the runtime/compiler authenticates as.
3. A schema-scoped publication (`FOR TABLES IN SCHEMA ...`) covering every schema referenced by `layers[*].sources[*].from`.
4. A logical replication slot using the `pgoutput` plugin.
5. `USAGE` on each published schema and `SELECT` on its current and future tables.

Tables only need a usable PRIMARY KEY (the postgres default replica identity). MARS no longer requires `REPLICA IDENTITY FULL`.

## Path A - automated bootstrap (default)

Set `spec.bootstrap` on the MarsService CR. The operator runs a one-shot `mars setup` Job before the compiler/runtime Deployments come up; on CR delete it runs a teardown Job that drops the slot/publication (and optionally the role) before letting the cascade complete.

```yaml
apiVersion: mars.forn.dk/v1alpha1
kind: MarsService
metadata:
  name: my-service
  namespace: maps
spec:
  bootstrap:
    enabled: true
    adminSecretRef:
      name: postgres-admin
      key: dsn
    runtimePasswordSecretRef:
      name: postgres-runtime
      key: password
    teardownOnDelete:
      slot: true
      publication: true
      role: false
  config:
    source:
      type: postgis
      dsn: "${PG_DSN}"
      native_crs: "EPSG:25832"
      change_feed:
        type: pgoutput
        publication: mars_pub
        slot: mars_slot
      bootstrap:
        role: mars_replicator
        schemas:
          - public
          - geo
    # ... rest of mars-config
```

What `mars setup` does (run inside one transaction except for the slot, which postgres requires outside):

1. `CREATE ROLE` (or `ALTER ROLE` if it exists) with `LOGIN REPLICATION` and the runtime password.
2. `GRANT USAGE` on each schema.
3. `GRANT SELECT ON ALL TABLES` in each schema.
4. `ALTER DEFAULT PRIVILEGES` so future tables in those schemas inherit `SELECT`. This is the load-bearing piece for swap-and-rename pipelines.
5. `CREATE PUBLICATION ... FOR TABLES IN SCHEMA ...` (or reconcile existing publication membership).
6. `pg_create_logical_replication_slot(...)`.

The admin DSN is mounted only into the bootstrap Job pod via `MARS_ADMIN_DSN`; the compiler/runtime Deployments never see it. Job names embed a content hash of the bootstrap-relevant fields, so a spec change spawns a new Job and the previous outcome stays visible.

While the bootstrap Job is running the CR carries `BootstrapReady=False, Reason=InProgress` and the compiler/runtime Deployments are not created. On success the condition flips to `Ready` and reconciliation proceeds. On failure the condition is `Failed` - inspect the Job's pod logs in the same namespace.

## Path B - manual bootstrap (opt-out)

Set `spec.bootstrap.enabled: false`. The operator skips the Job and assumes the catalog state is already in place.

```yaml
spec:
  bootstrap:
    enabled: false
  config:
    source:
      # same as Path A; bootstrap.role and bootstrap.schemas are still
      # consulted by `mars setup --dry-run` if you want a paste-ready SQL
      # reference.
      ...
```

Run the equivalent SQL by hand (these are exactly the statements `mars setup --dry-run` prints):

```sql
-- 1. Role
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'mars_replicator') THEN
    CREATE ROLE "mars_replicator" WITH LOGIN REPLICATION PASSWORD '<runtime password>';
  ELSE
    ALTER ROLE "mars_replicator" WITH LOGIN REPLICATION PASSWORD '<runtime password>';
  END IF;
END
$$;

-- 2-4. Grants + default privileges (per schema)
GRANT USAGE ON SCHEMA "public" TO "mars_replicator";
GRANT SELECT ON ALL TABLES IN SCHEMA "public" TO "mars_replicator";
ALTER DEFAULT PRIVILEGES IN SCHEMA "public" GRANT SELECT ON TABLES TO "mars_replicator";
GRANT USAGE ON SCHEMA "geo" TO "mars_replicator";
GRANT SELECT ON ALL TABLES IN SCHEMA "geo" TO "mars_replicator";
ALTER DEFAULT PRIVILEGES IN SCHEMA "geo" GRANT SELECT ON TABLES TO "mars_replicator";

-- 5. Publication
CREATE PUBLICATION "mars_pub" FOR TABLES IN SCHEMA "public", "geo";

-- 6. Slot
SELECT pg_create_logical_replication_slot('mars_slot', 'pgoutput');
```

Bare-metal deployments of MARS (no operator) use the same `mars setup` CLI: provide the admin DSN via env or `--admin-dsn`, the runtime password via env or `--runtime-password`, and a config file with `source.bootstrap` set. `mars teardown --drop-slot --drop-publication` is the inverse.

## Day-2 diagnostics

### Slot health

```sql
SELECT slot_name,
       active,
       confirmed_flush_lsn,
       restart_lsn,
       pg_size_pretty(pg_wal_lsn_diff(pg_current_wal_lsn(), restart_lsn)) AS retained_wal
FROM pg_replication_slots
WHERE slot_name = 'mars_slot';
```

`retained_wal` growing without bound means the compiler is not advancing its cursor. Either the compiler is down or its publish path is failing (manifest publish must succeed before the cursor advances).

### Publication membership

```sql
SELECT n.nspname AS schema
FROM pg_publication_namespace pn
JOIN pg_namespace n ON n.oid = pn.pnnspid
JOIN pg_publication p ON p.oid = pn.pnpubid
WHERE p.pubname = 'mars_pub'
ORDER BY 1;
```

The list should match `source.bootstrap.schemas`. The automated path reconciles this on every apply via `ALTER PUBLICATION ... ADD/DROP TABLES IN SCHEMA`; the manual path is the operator's responsibility.

### Replica identity

A bound table whose id column is not part of its PK / `REPLICA IDENTITY USING INDEX` will be rejected at preflight. Default postgres tables with a PK satisfy this automatically.

```sql
SELECT n.nspname || '.' || c.relname AS rel,
       CASE c.relreplident
            WHEN 'd' THEN 'default (primary key)'
            WHEN 'n' THEN 'nothing'
            WHEN 'f' THEN 'full'
            WHEN 'i' THEN 'index'
       END AS replica_identity
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_publication_namespace pn ON pn.pnnspid = n.oid
JOIN pg_publication p ON p.oid = pn.pnpubid
WHERE p.pubname = 'mars_pub'
  AND c.relkind = 'r';
```

## Failure modes the service surfaces

The compiler returns errors via the `SourceError::Backend` channel; they are logged with the layer/relation context. The expected ones, with the fix:

| Message fragment | Fix |
|---|---|
| `replication connect: ... slot=... publication=...` | Slot or publication does not exist, or the connecting user lacks `REPLICATION`. Re-run `mars setup` (automated) or check `pg_replication_slots` / `pg_publication` (manual). |
| `pgoutput: row for unknown relation oid <N>` | The publication is changing under the running compiler. Restart the compiler (its relation cache is per-session). |
| `dsn: unsupported sslmode for replication: VerifyCa` | libpq DSN cannot express stronger TLS than `require`; configure peer certificates at the network layer or use a TLS-terminating proxy in front of the DB. |

## Snapshot vs incremental

A fresh compiler with no local manifest does a snapshot compile from PostGIS first and only then opens the replication subscription. Operators can force a snapshot rebuild by deleting the local manifest from the artifact store; the next compiler start will rebuild and resume incremental from the slot's current `confirmed_flush_lsn`.

If the slot has fallen too far behind for the WAL retention window, PostgreSQL will drop it and the compiler will surface `change feed gone; full snapshot required`. Recreate the slot and the manifest:

```sql
SELECT pg_drop_replication_slot('mars_slot');
SELECT pg_create_logical_replication_slot('mars_slot', 'pgoutput');
```

then delete the local manifest and restart the compiler. The automated path reaches the same end state via `mars teardown --drop-slot` followed by a re-applied bootstrap.

# PostgreSQL setup for the MARS compiler

The compiler streams logical replication via `pgoutput` from the source PostgreSQL instance. This document is the operator runbook: what the service expects, how to configure the DB, and the failure modes the service surfaces if something is missing.

The technical contract is owned by `SPEC.md` §8.2.1. This document does not override it; it spells out the day-1 setup steps and the day-2 diagnostics.

## What the service expects

For one MARS service (one logical-replication subscription) the source DB must have:

1. `wal_level = logical` cluster-wide, and the postmaster restarted to apply.
2. A user with the `REPLICATION` attribute that the compiler authenticates as.
3. A publication named in `source.change_feed.publication` covering every table referenced by `layers[*].sources[*].from`.
4. An existing logical replication slot named in `source.change_feed.slot`, using the `pgoutput` output plugin.
5. `REPLICA IDENTITY FULL` on every table covered by the publication.

The compiler does not create the publication or the slot — those are operator-owned because their lifetime is tied to the data, not the service deployment. Recreating the slot drops all uncommitted change history; the compiler will fall back to a snapshot rebuild on next start.

## Day-1 setup

```sql
-- 1. WAL level. Requires a postmaster restart.
ALTER SYSTEM SET wal_level = 'logical';
-- (restart the cluster here)
SHOW wal_level;  -- expect: logical

-- 2. Replication role.
CREATE ROLE mars LOGIN PASSWORD '...' REPLICATION;
GRANT USAGE ON SCHEMA my_schema TO mars;
GRANT SELECT ON ALL TABLES IN SCHEMA my_schema TO mars;
ALTER DEFAULT PRIVILEGES IN SCHEMA my_schema GRANT SELECT ON TABLES TO mars;

-- 3. Publication. List every bound table explicitly so adding a layer is an
--    intentional step, not an accident.
CREATE PUBLICATION mars_my_service FOR TABLE
    my_schema.parcels,
    my_schema.buildings
    WITH (publish = 'insert, update, delete, truncate');

-- 4. REPLICA IDENTITY FULL on each bound table. SPEC §8.2.1: required so
--    UPDATEs and DELETEs carry the old geometry; without it, MARS cannot
--    invalidate the cells of moved-or-deleted features and the rendered
--    output drifts from source.
ALTER TABLE my_schema.parcels REPLICA IDENTITY FULL;
ALTER TABLE my_schema.buildings REPLICA IDENTITY FULL;

-- 5. Logical replication slot.
SELECT pg_create_logical_replication_slot('mars_my_service', 'pgoutput');
```

The MARS YAML refers to these by name:

```yaml
source:
  type: postgis
  dsn: "postgres://mars@db.example/mars?sslmode=require"
  change_feed:
    type: pgoutput
    publication: mars_my_service
    slot: mars_my_service
```

## Day-2 diagnostics

### Slot health

```sql
SELECT slot_name,
       active,
       confirmed_flush_lsn,
       restart_lsn,
       pg_size_pretty(pg_wal_lsn_diff(pg_current_wal_lsn(), restart_lsn)) AS retained_wal
FROM pg_replication_slots
WHERE slot_name = 'mars_my_service';
```

`retained_wal` growing without bound means the compiler is not advancing its cursor. Either the compiler is down or its publish path is failing (manifest publish must succeed before the cursor advances; SPEC §8.3).

### Publication membership

```sql
SELECT schemaname || '.' || tablename AS rel
FROM pg_publication_tables
WHERE pubname = 'mars_my_service'
ORDER BY 1;
```

A table referenced by a layer but missing from the publication produces a silent gap: rows in that table never reach the compiler. Add it with `ALTER PUBLICATION mars_my_service ADD TABLE schema.table`, then run a snapshot rebuild to backfill.

### REPLICA IDENTITY

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
WHERE c.oid IN (
    SELECT c.oid FROM pg_publication_rel pr
    JOIN pg_class c ON c.oid = pr.prrelid
    JOIN pg_publication p ON p.oid = pr.prpubid
    WHERE p.pubname = 'mars_my_service'
);
```

Anything other than `full` for a published table will surface at runtime as a hard `Backend` error from the compiler the first time an UPDATE or DELETE hits it.

## Failure modes the service surfaces

The compiler returns errors via the `SourceError::Backend` channel; they are logged with the layer/relation context. The expected ones, with the fix:

| Message fragment | Fix |
|---|---|
| `replication connect: ... slot=... publication=...` | Slot or publication does not exist, or the connecting user lacks `REPLICATION`. Check `pg_replication_slots` / `pg_publication`. |
| `update on schema.table requires REPLICA IDENTITY FULL (got identity 'd')` | `ALTER TABLE schema.table REPLICA IDENTITY FULL`. |
| `delete on schema.table requires REPLICA IDENTITY FULL ...` | Same as above. |
| `pgoutput: row for unknown relation oid <N>` | The publication is changing under the running compiler. Restart the compiler (its relation cache is per-session). |
| `dsn: unsupported sslmode for replication: VerifyCa` | libpq DSN cannot express stronger TLS than `require`; configure peer certificates at the network layer or use a TLS-terminating proxy in front of the DB. |

## Snapshot vs incremental

A fresh compiler with no local manifest does a snapshot compile from PostGIS first (SPEC §8.2.3) and only then opens the replication subscription. Operators can force a snapshot rebuild by deleting the local manifest from the artifact store; the next compiler start will rebuild and resume incremental from the slot's current `confirmed_flush_lsn`.

If the slot has fallen too far behind for the WAL retention window, PostgreSQL will drop it and the compiler will surface `change feed gone; full snapshot required`. The fix is to recreate the slot and the manifest:

```sql
SELECT pg_drop_replication_slot('mars_my_service');
SELECT pg_create_logical_replication_slot('mars_my_service', 'pgoutput');
```

then delete the local manifest and restart the compiler.

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

Declare `bootstrap` on the `MarsServiceCluster` source catalog entry. The cluster reconciler runs a one-shot `mars setup` Job before any service composing against that catalog needs the publication and slot.

```yaml
apiVersion: mars.forn.dk/v1alpha1
kind: MarsServiceCluster
metadata:
  name: prod-eu
spec:
  sourcesCatalog:
    - id: default
      type: postgis
      dsn: "postgresql://mars_replicator:${MARS_RUNTIME_PASSWORD}@postgis:5432/maps"
      native_crs: "EPSG:25832"
      change_feed:
        type: pgoutput
        publication: mars_pub
        slot: mars_slot
      bootstrap:
        enabled: true
        adminSecretRef:
          name: postgres-admin
          key: dsn
        # runtimePasswordSecretRef is OPTIONAL. Omit it and the operator
        # generates a 32-char random password on first reconcile and stores
        # it in `<cluster>-<source>-runtime-credentials` (key `password`)
        # with an owner reference back to this MarsServiceCluster.
        teardownOnDelete:
          slot: true
          publication: true
          role: false
        role: mars_replicator
        schemas: [public, geo]
  artifactStore:
    store: { type: s3, bucket: mars-artifacts, region: eu-west-1 }
    cache: { path: /cache, max_size: 1GiB, eviction: lru }
```

What `mars setup` does (run inside one transaction except for the slot, which postgres requires outside):

1. `CREATE ROLE` (or `ALTER ROLE` if it exists) with `LOGIN REPLICATION` and the runtime password.
2. `GRANT USAGE` on each schema.
3. `GRANT SELECT ON ALL TABLES` in each schema.
4. `ALTER DEFAULT PRIVILEGES` so future tables in those schemas inherit `SELECT`. This is the load-bearing piece for swap-and-rename pipelines.
5. `CREATE PUBLICATION ... FOR TABLES IN SCHEMA ...` (or reconcile existing publication membership).
6. `pg_create_logical_replication_slot(...)`.

The admin DSN is mounted only into the bootstrap Job pod via `MARS_ADMIN_DSN`; the compiler/runtime Deployments never see it. Job names embed a content hash of the bootstrap-relevant fields, so a spec change spawns a new Job and the previous outcome stays visible.

### Runtime password: operator-managed vs BYO

`bootstrap.runtimePasswordSecretRef` is the bring-your-own escape hatch for installs that source credentials from an external secrets manager (Vault, SOPS, sealed-secrets, ESO). When set, behaviour is unchanged: the user owns the Secret end-to-end and the operator never writes to it.

When omitted, the operator manages a single Secret per (cluster, source):

- Name: `<cluster>-<source>-runtime-credentials`. Key: `password`.
- OwnerReference: the MarsServiceCluster, so deleting the cluster CR garbage-collects it.
- Generated once on first reconcile (32 chars from `[A-Za-z0-9]`, ~190 bits of entropy). Never rotated in-place.
- Consumed by the bootstrap Job to set the role password. The compiler/runtime pods are responsible for projecting it into their own env (via `spec.compiler.env` / `spec.runtime.env` with `valueFrom.secretKeyRef`).

**Rotation:** delete the managed Secret and let reconcile regenerate it; the next bootstrap Job applies the new password via `ALTER ROLE`. The old password persists in postgres until that Job runs.

### Admin credentials: single-DSN vs component-style

The admin role is consumed by `mars setup` only and only by the Job pod. The catalog entry accepts the credential in either shape, mutually exclusive; exactly one must be set when `enabled` is true:

```yaml
# Form 1: single Secret key holds a complete libpq URI. Preferred for
# non-Kubernetes postgres (RDS, bare metal) where the operator just gets
# a URL.
sourcesCatalog:
  - id: default
    bootstrap:
      adminSecretRef:
        name: postgres-admin
        key: dsn

# Form 2: separate Secret keys for username / password / (optional)
# host / port / database. Preferred when a Postgres operator (CNPG,
# Zalando, Crunchy) is in play, since those emit credentials as
# multi-key Secrets.
sourcesCatalog:
  - id: default
    bootstrap:
      adminCredentialsRef:
        secretName: pg-cluster-superuser
        usernameKey: username     # defaults to "username" (CNPG shape)
        passwordKey: password     # defaults to "password" (CNPG shape)
        # host / port / database default to the values parsed out of the
        # catalog entry's `dsn` so a single config-level DSN can supply
        # connection targeting. Override per-field if needed:
        # hostKey: host
        # portKey: port
        # databaseKey: database
```

With `adminCredentialsRef` the operator reads the referenced Secret, composes a libpq URI by combining its keys with whatever host/port/database it can parse out of the catalog entry's `dsn` (templated values like `${PG_DSN}` are tolerated and skipped), persists the composed DSN into a managed `<cluster>-<source>-bootstrap-admin-credentials` Secret, and passes that as `MARS_ADMIN_DSN` on the Job container.

## Path B - manual bootstrap (opt-out)

Set `bootstrap.enabled: false` on the catalog entry, or omit the `bootstrap` block entirely. The operator skips the Job and assumes the catalog state is already in place.

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

Bare-metal deployments of MARS (no operator) use the same `mars setup` CLI: provide the admin DSN via env or `--admin-dsn`, the runtime password via env or `--runtime-password`, and a config file with `sources[].bootstrap` set. `mars teardown --drop-slot --drop-publication` is the inverse.

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

The list should match `sources[].bootstrap.schemas`. The automated path reconciles this on every apply via `ALTER PUBLICATION ... ADD/DROP TABLES IN SCHEMA`; the manual path is the operator's responsibility.

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
